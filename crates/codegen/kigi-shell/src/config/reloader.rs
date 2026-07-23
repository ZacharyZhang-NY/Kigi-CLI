use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::auth::{KimiAuth, read_auth_json};

use super::watcher::ConfigChangeEvent;

/// Typed, `Send`-safe messages for the agent to apply inside its `LocalSet`.
#[derive(Debug)]
pub enum ConfigUpdate {
    /// New auth credentials from disk.
    Auth(Box<KimiAuth>),
    /// Auth scope was removed (user logged out).
    AuthCleared,
    /// A **broadcast** MCP reload — applies to every active session
    /// regardless of cwd. Fires for two cases:
    ///
    /// 1. The global `[mcp_servers]` table in `~/.kigi/config.toml`
    ///    changed.
    /// 2. The user's home-level `~/.claude.json` changed.
    ///    `load_claude_json_mcp_servers_as_configs` reads this file
    ///    for every session, so the reload cannot be narrowed by cwd.
    ///
    /// Project-scoped changes (`<cwd>/.kigi/config.toml`,
    /// `<cwd>/.mcp.json`, project-level `<cwd>/.claude.json`) emit
    /// [`Self::ProjectMcpServersChanged`] instead so the reload can
    /// be narrowed to matching cwds.
    ///
    /// Deliberately a unit variant — a payload would force pattern-match
    /// updates across `mvp_agent`, `app`, `session/handle`, etc.
    McpServersChanged,
    /// A **project-scoped** MCP config file (`<cwd>/.kigi/config.toml`,
    /// `<cwd>/.mcp.json`, or `<cwd>/.claude.json`) changed. Agent should
    /// reload MCP only for sessions whose cwd matches `cwd` (or sits
    /// beneath it).
    ///
    /// Additive to [`Self::McpServersChanged`] — the unit variant still
    /// fires for global-config edits. The two cases are split so
    /// per-project reloads don't thrash unrelated sessions.
    ProjectMcpServersChanged {
        /// The project root whose `.kigi/`, `.mcp.json`, or
        /// `.claude.json` file was edited. Sessions whose cwd equals
        /// this path — or is a descendant of it — are the reload
        /// targets.
        cwd: PathBuf,
    },
    /// Updated memory config (boxed to avoid large enum variant).
    Memory(Box<crate::config::MemoryConfig>),
    /// Updated skills discovery config.
    Skills(kigi_agent::prompt::skills::SkillsConfig),
    /// Updated `[compat]` vendor-compatibility config. Applied on the
    /// next agent (re)build, which re-resolves `compat_resolved`.
    Compat(Box<kigi_tools::types::compat::CompatConfigToml>),
    /// The `[model.*]` entries in config.toml changed. Agent should re-resolve
    /// its model list (BYOK models added/removed, default or surprise changed).
    ModelsChanged,
    /// `~/.kigi/models_cache.json` was rewritten on disk (possibly by another
    /// kigi process sharing the home dir). No payload — validation (TTL,
    /// version, auth method) requires `ModelsManager` state the reloader
    /// doesn't have. The agent forwards to `ModelsManager::reload_from_disk_cache`,
    /// which content-dedupes self-writes (`persist` / `renew_ttl`) before applying.
    ModelsCacheChanged,
    /// Updated UI settings — agent broadcasts `kigi/config_changed` to IPC clients.
    Ui {
        theme: Option<String>,
        yolo: bool,
        fork_secondary_model: Option<String>,
    },
}

/// Runs on `tokio::spawn` (`Send`). Receives raw [`ConfigChangeEvent`]s from
/// the file watcher, diffs against last-known state, and sends [`ConfigUpdate`]
/// messages to the agent via an `mpsc` channel.
pub struct ConfigReloader {
    last_auth_key_hash: u64,
    last_global_config: toml::Value,
    /// Per-cwd content hash of the project MCP config files, used to drop
    /// redundant `ProjectMcpServersChanged` dispatches on mtime-only touches
    /// (see `hash_project_mcp_config`).
    last_project_mcp_hashes: HashMap<PathBuf, u64>,
    kigi_home: PathBuf,
    auth_scope: String,
    remote_settings: Option<crate::util::config::RemoteSettings>,
    config_update_tx: mpsc::UnboundedSender<ConfigUpdate>,
    /// Whether --experimental-memory was passed at startup. Persists across config reloads.
    experimental_memory: bool,
    /// Whether --no-memory was passed at startup. Persists across config reloads.
    no_memory: bool,
}

impl ConfigReloader {
    pub fn new(
        kigi_home: PathBuf,
        initial_auth_key_hash: u64,
        initial_config: toml::Value,
        auth_scope: String,
        remote_settings: Option<crate::util::config::RemoteSettings>,
        config_update_tx: mpsc::UnboundedSender<ConfigUpdate>,
        experimental_memory: bool,
        no_memory: bool,
    ) -> Self {
        Self {
            last_auth_key_hash: initial_auth_key_hash,
            last_global_config: initial_config,
            last_project_mcp_hashes: HashMap::new(),
            kigi_home,
            auth_scope,
            remote_settings,
            config_update_tx,
            experimental_memory,
            no_memory,
        }
    }

    /// Main loop. Batches all events from each debounce tick before processing.
    pub async fn run(
        mut self,
        mut events: mpsc::UnboundedReceiver<ConfigChangeEvent>,
        cancel: CancellationToken,
    ) {
        loop {
            let first = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                evt = events.recv() => match evt {
                    Some(e) => e,
                    None => break,
                },
            };

            // Drain additional events that arrived in the same tick
            let mut batch = vec![first];
            while let Ok(evt) = events.try_recv() {
                batch.push(evt);
            }

            let has_auth = batch
                .iter()
                .any(|e| matches!(e, ConfigChangeEvent::AuthChanged));
            let has_global_config = batch
                .iter()
                .any(|e| matches!(e, ConfigChangeEvent::GlobalConfigChanged));
            let has_project_config = batch
                .iter()
                .any(|e| matches!(e, ConfigChangeEvent::ProjectConfigChanged { .. }));
            let has_home_claude_json = batch
                .iter()
                .any(|e| matches!(e, ConfigChangeEvent::HomeClaudeJsonChanged));
            let has_models_cache = batch
                .iter()
                .any(|e| matches!(e, ConfigChangeEvent::ModelsCacheChanged));
            let has_config = has_global_config || has_project_config;

            let project_cwds = collect_project_cwds(&batch);

            if has_auth {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.reload_auth()));
                match result {
                    Ok(Err(e)) => {
                        error!(error = %e, "auth hot-reload failed, keeping previous credentials");
                        // Whole-file deletion (NotFound) and corrupt JSON
                        // land here. The resulting memory/disk divergence
                        // must be visible in unified.jsonl.
                        let path = self.kigi_home.join("auth.json");
                        kigi_log::unified_log::error(
                            "auth reload: auth.json unreadable, keeping previous credentials",
                            None,
                            Some(serde_json::json!({
                                "error": e.to_string(),
                                "path": path.display().to_string(),
                                "path_exists": path.exists(),
                            })),
                        );
                    }
                    Err(_) => {
                        error!("panic in auth reload handler, keeping previous credentials");
                    }
                    Ok(Ok(())) => {}
                }
            }

            if has_config {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.reload_config()));
                match result {
                    Ok(Err(e)) => {
                        error!(error = %e, "config hot-reload failed, keeping last-known-good");
                    }
                    Err(_) => {
                        error!("panic in config reload handler, keeping last-known-good");
                    }
                    Ok(Ok(())) => {}
                }
            }

            // `~/.claude.json` is read by every session, so its reload must
            // broadcast; the per-cwd filter would skip sessions outside `$HOME`.
            if has_home_claude_json {
                info!("~/.claude.json change detected — broadcasting MCP reload");
                let _ = self.config_update_tx.send(ConfigUpdate::McpServersChanged);
            }

            // Pass-through: no toml to diff here; the content dedup happens in
            // `ModelsManager::reload_from_disk_cache`.
            if has_models_cache {
                debug!("models_cache.json change detected — forwarding to agent");
                let _ = self.config_update_tx.send(ConfigUpdate::ModelsCacheChanged);
            }

            // One `ProjectMcpServersChanged { cwd }` per affected project root.
            // Both variants can fire in the same tick (e.g. `~/.kigi/config.toml`
            // and `<cwd>/.mcp.json` edited together).
            for cwd in project_cwds {
                // Skip the dispatch when the config bytes are unchanged (the
                // watcher fires on mtime-only touches). On any uncertainty
                // `hash_project_mcp_config` returns `None` and we dispatch.
                let new_hash = hash_project_mcp_config(&cwd);
                let unchanged = match (new_hash, self.last_project_mcp_hashes.get(&cwd)) {
                    (Some(new), Some(&prev)) => new == prev,
                    _ => false,
                };
                if unchanged {
                    debug!(
                        cwd = %cwd.display(),
                        "project MCP config event with unchanged content, skipping reload"
                    );
                    continue;
                }
                if let Some(h) = new_hash {
                    self.last_project_mcp_hashes.insert(cwd.clone(), h);
                }
                info!("project MCP config change detected");
                let _ = self
                    .config_update_tx
                    .send(ConfigUpdate::ProjectMcpServersChanged { cwd });
            }
        }
    }

    fn reload_auth(&mut self) -> anyhow::Result<()> {
        let auth_path = self.kigi_home.join("auth.json");
        let store = read_auth_json(&auth_path)?;

        match crate::auth::lookup_auth(&store, &self.auth_scope) {
            Some(auth) => {
                let new_hash = hash_auth_key(&auth.key);

                if new_hash == self.last_auth_key_hash {
                    debug!("auth.json changed but token key is identical, skipping");
                    return Ok(());
                }

                self.last_auth_key_hash = new_hash;
                let _ = self
                    .config_update_tx
                    .send(ConfigUpdate::Auth(Box::new(auth.clone())));
                info!("auth token change detected, sent update to agent");
            }
            None => {
                if self.last_auth_key_hash != 0 {
                    self.last_auth_key_hash = 0;
                    let _ = self.config_update_tx.send(ConfigUpdate::AuthCleared);
                    info!("auth scope removed from auth.json, sent clear to agent");
                    // AuthCleared makes the agent drop in-memory credentials;
                    // record what the reloader saw so "entry removed" is
                    // distinguishable from "file deleted" (the Err path).
                    kigi_log::unified_log::warn(
                        "auth reload: scope entry gone, sending AuthCleared",
                        None,
                        Some(serde_json::json!({
                            "scope": &self.auth_scope,
                            "scopes_on_disk": store.keys().collect::<Vec<_>>(),
                        })),
                    );
                }
            }
        }
        Ok(())
    }

    fn reload_config(&mut self) -> anyhow::Result<()> {
        // Diffs only the global toml; project-scoped reloads go through the
        // caller's `collect_project_cwds` fan-out.
        let new_global = match crate::config::load_from_disk() {
            Ok(v) => v,
            Err(e) => {
                error!(error = %e, "failed to parse config.toml, keeping last-known-good");
                return Ok(());
            }
        };

        let old_mcp_table = self.last_global_config.get("mcp_servers");
        let new_mcp_table = new_global.get("mcp_servers");
        let mcp_changed = old_mcp_table != new_mcp_table;
        if mcp_changed {
            info!("Global MCP server config change detected");
            let _ = self.config_update_tx.send(ConfigUpdate::McpServersChanged);
        }

        let old_mem = crate::config::MemoryConfig::resolve(
            self.experimental_memory,
            self.no_memory,
            &self.last_global_config,
            self.remote_settings.as_ref(),
        );
        let new_mem = crate::config::MemoryConfig::resolve(
            self.experimental_memory,
            self.no_memory,
            &new_global,
            self.remote_settings.as_ref(),
        );
        if old_mem != new_mem {
            info!("memory config change detected");
            let _ = self
                .config_update_tx
                .send(ConfigUpdate::Memory(Box::new(new_mem)));
        }

        let old_skills = parse_skills_config(&self.last_global_config);
        let new_skills = parse_skills_config(&new_global);
        if old_skills != new_skills {
            info!("skills config change detected");
            let _ = self.config_update_tx.send(ConfigUpdate::Skills(new_skills));
        }

        let old_compat = parse_compat_config(&self.last_global_config);
        let new_compat = parse_compat_config(&new_global);
        if old_compat != new_compat {
            info!("compat config change detected");
            let _ = self
                .config_update_tx
                .send(ConfigUpdate::Compat(Box::new(new_compat)));
        }

        // `[model]` = BYOK entries, `[models]` = default/surprise — two distinct tables.
        let old_model_table = self.last_global_config.get("model");
        let new_model_table = new_global.get("model");
        let old_models_table = self.last_global_config.get("models");
        let new_models_table = new_global.get("models");
        if old_model_table != new_model_table || old_models_table != new_models_table {
            info!("model config change detected");
            let _ = self.config_update_tx.send(ConfigUpdate::ModelsChanged);
        }

        let old_ui = extract_ui_fields(&self.last_global_config);
        let new_ui = extract_ui_fields(&new_global);
        if old_ui != new_ui {
            info!("UI config change detected");
            let _ = self.config_update_tx.send(ConfigUpdate::Ui {
                theme: new_ui.0,
                yolo: new_ui.1,
                fork_secondary_model: new_ui.2,
            });
        }

        self.last_global_config = new_global;
        Ok(())
    }
}

/// Derive the unique project cwds whose files were touched in this
/// debounce window, to fan out one
/// [`ConfigUpdate::ProjectMcpServersChanged`] per project root rather
/// than one `McpServersChanged` that reloads every active session.
///
/// Order-preserving de-dup (a `Vec` rather than a `HashSet`) so the
/// downstream emit order is deterministic in tests.
fn collect_project_cwds(batch: &[ConfigChangeEvent]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for evt in batch {
        let cwd = match evt {
            ConfigChangeEvent::ProjectConfigChanged { path } => {
                // <cwd>/.kigi/config.toml → <cwd>
                path.parent()
                    .and_then(|p| p.parent())
                    .map(|p| p.to_path_buf())
            }
            ConfigChangeEvent::McpConfigChanged { path } => {
                // <cwd>/.mcp.json or <cwd>/.claude.json → <cwd>
                path.parent().map(|p| p.to_path_buf())
            }
            _ => None,
        };
        if let Some(cwd) = cwd
            && !out.contains(&cwd)
        {
            out.push(cwd);
        }
    }
    out
}

/// Content hash of the cwd-dependent MCP config files a
/// `ProjectMcpServersChanged { cwd }` reload re-reads. Walks ancestors
/// up to the git root exactly as the loaders do (`find_project_configs`
/// for `.kigi/config.toml`, `find_mcp_json_files` for `.mcp.json`) so
/// the hash can't drift from the set the merge actually reads, plus
/// `<cwd>/.claude.json` (watched at the project root). A stable hash
/// means the reload would be a no-op. Home-level sources
/// (`~/.kigi/config.toml`, `~/.claude.json`, `~/.cursor/mcp.json`)
/// change through their own events.
///
/// Returns `None` on a non-`NotFound` read error so the caller
/// dispatches rather than risk suppressing a real edit.
fn hash_project_mcp_config(cwd: &Path) -> Option<u64> {
    let mut paths = crate::config::find_project_configs(cwd);
    paths.extend(crate::util::config::find_mcp_json_files(cwd));
    paths.push(cwd.join(".claude.json"));

    let mut hasher = DefaultHasher::new();
    paths.len().hash(&mut hasher);
    for f in &paths {
        f.to_string_lossy().hash(&mut hasher);
        match std::fs::read(f) {
            Ok(bytes) => {
                // present
                1u8.hash(&mut hasher);
                bytes.hash(&mut hasher);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // absent
                0u8.hash(&mut hasher);
            }
            // can't read confidently → dispatch
            Err(_) => return None,
        }
    }
    Some(hasher.finish())
}

pub(crate) fn hash_auth_key(key: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

/// Extract the `[skills]` table from an effective config.
///
/// Consumers: the reload dispatch above (change detection →
/// `ConfigUpdate::Skills`) and `kigi inspect` (via the `crate::config`
/// re-export), so both honor the same paths/ignore/disabled as a live
/// session. Session spawn parses the same table separately through the typed
/// `Config.skills` (agent/config.rs) — keep these in sync rather than adding
/// a fourth parse path.
pub(crate) fn parse_skills_config(
    config: &toml::Value,
) -> kigi_agent::prompt::skills::SkillsConfig {
    config
        .get("skills")
        .and_then(|v| v.clone().try_into().ok())
        .unwrap_or_default()
}

fn parse_compat_config(config: &toml::Value) -> kigi_tools::types::compat::CompatConfigToml {
    config
        .get("compat")
        .and_then(|v| v.clone().try_into().ok())
        .unwrap_or_default()
}

fn extract_ui_fields(config: &toml::Value) -> (Option<String>, bool, Option<String>) {
    let ui = config.get("ui").and_then(|v| v.as_table());
    let theme = ui
        .and_then(|u| u.get("theme"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let yolo = ui
        .and_then(|u| u.get("yolo"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let fork = ui
        .and_then(|u| u.get("fork_secondary_model"))
        .and_then(|v| v.as_str())
        .map(String::from);
    (theme, yolo, fork)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::KimiAuth;
    use std::collections::BTreeMap;

    fn make_auth(key: &str) -> KimiAuth {
        KimiAuth {
            key: key.to_string(),
            email: Some("test@test.com".to_string()),
            ..KimiAuth::test_default()
        }
    }

    #[tokio::test]
    async fn reloader_skips_unchanged_auth() {
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = make_auth("same-key");
        let mut store = BTreeMap::new();
        let scope = "https://test.example.com".to_string();
        store.insert(scope.clone(), auth);
        let json = serde_json::to_string_pretty(&store).unwrap();
        std::fs::write(tmp.path().join("auth.json"), &json).unwrap();

        let initial_hash = hash_auth_key("same-key");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let empty_config = toml::Value::Table(toml::map::Map::new());
        let mut reloader = ConfigReloader::new(
            tmp.path().to_path_buf(),
            initial_hash,
            empty_config,
            scope,
            None,
            tx,
            false,
            false,
        );

        reloader.reload_auth().unwrap();
        assert!(
            rx.try_recv().is_err(),
            "should not send update when key is unchanged"
        );
    }

    #[tokio::test]
    async fn reloader_detects_new_auth_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = make_auth("new-key");
        let mut store = BTreeMap::new();
        let scope = "https://test.example.com".to_string();
        store.insert(scope.clone(), auth);
        let json = serde_json::to_string_pretty(&store).unwrap();
        std::fs::write(tmp.path().join("auth.json"), &json).unwrap();

        let old_hash = hash_auth_key("old-key");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let empty_config = toml::Value::Table(toml::map::Map::new());
        let mut reloader = ConfigReloader::new(
            tmp.path().to_path_buf(),
            old_hash,
            empty_config,
            scope,
            None,
            tx,
            false,
            false,
        );

        reloader.reload_auth().unwrap();
        let update = rx.try_recv().expect("should send Auth update");
        assert!(
            matches!(update, ConfigUpdate::Auth(a) if a.key == "new-key"),
            "should contain new key"
        );
    }

    #[tokio::test]
    async fn reloader_detects_auth_cleared() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Write auth.json with a DIFFERENT scope — our scope is missing
        let auth = make_auth("other-key");
        let mut store = BTreeMap::new();
        store.insert("https://other.example.com".to_string(), auth);
        let json = serde_json::to_string_pretty(&store).unwrap();
        std::fs::write(tmp.path().join("auth.json"), &json).unwrap();

        let old_hash = hash_auth_key("had-a-key");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let empty_config = toml::Value::Table(toml::map::Map::new());
        let mut reloader = ConfigReloader::new(
            tmp.path().to_path_buf(),
            old_hash,
            empty_config,
            "https://test.example.com".to_string(),
            None,
            tx,
            false,
            false,
        );

        reloader.reload_auth().unwrap();
        let update = rx.try_recv().expect("should send AuthCleared");
        assert!(matches!(update, ConfigUpdate::AuthCleared));
    }

    #[tokio::test]
    async fn reloader_handles_malformed_auth_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("auth.json"), "not valid json{{{").unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let empty_config = toml::Value::Table(toml::map::Map::new());
        let mut reloader = ConfigReloader::new(
            tmp.path().to_path_buf(),
            0,
            empty_config,
            "https://test.example.com".to_string(),
            None,
            tx,
            false,
            false,
        );

        let result = reloader.reload_auth();
        assert!(result.is_err(), "malformed JSON should return error");
        assert!(
            rx.try_recv().is_err(),
            "should not send update on parse failure"
        );
    }

    #[tokio::test]
    async fn reloader_handles_missing_auth_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        // No auth.json written

        let (tx, mut rx) = mpsc::unbounded_channel();
        let empty_config = toml::Value::Table(toml::map::Map::new());
        let mut reloader = ConfigReloader::new(
            tmp.path().to_path_buf(),
            0,
            empty_config,
            "https://test.example.com".to_string(),
            None,
            tx,
            false,
            false,
        );

        let result = reloader.reload_auth();
        assert!(result.is_err(), "missing file should return error");
        assert!(
            rx.try_recv().is_err(),
            "should not send update on missing file"
        );
    }

    /// `ModelsCacheChanged` is a pure pass-through: the reloader has no toml to
    /// diff, so the event must surface as `ConfigUpdate::ModelsCacheChanged`
    /// without touching auth or config state.
    #[tokio::test]
    async fn reloader_forwards_models_cache_changed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let empty_config = toml::Value::Table(toml::map::Map::new());
        let reloader = ConfigReloader::new(
            tmp.path().to_path_buf(),
            0,
            empty_config,
            "https://test.example.com".to_string(),
            None,
            tx,
            false,
            false,
        );

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(reloader.run(event_rx, cancel.clone()));

        event_tx
            .send(ConfigChangeEvent::ModelsCacheChanged)
            .unwrap();

        let update = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("should receive an update within 2s")
            .expect("update channel should remain open");
        assert!(matches!(update, ConfigUpdate::ModelsCacheChanged));

        cancel.cancel();
        let _ = handle.await;
    }

    /// A project event with unchanged bytes must not re-dispatch a
    /// reload; the first event and a later real edit must both dispatch.
    #[tokio::test]
    async fn reloader_dedupes_unchanged_project_mcp_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let cwd = tmp.path().to_path_buf();
        let mcp_json = cwd.join(".mcp.json");
        std::fs::write(&mcp_json, r#"{"mcpServers":{}}"#).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let empty_config = toml::Value::Table(toml::map::Map::new());
        let reloader = ConfigReloader::new(
            tmp.path().to_path_buf(),
            0,
            empty_config,
            "https://test.example.com".to_string(),
            None,
            tx,
            false,
            false,
        );

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(reloader.run(event_rx, cancel.clone()));

        let evt = || ConfigChangeEvent::McpConfigChanged {
            path: mcp_json.clone(),
        };

        // First event → dispatch (no prior hash for this cwd).
        event_tx.send(evt()).unwrap();
        let update = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("first event should dispatch within 2s")
            .expect("channel open");
        assert!(
            matches!(update, ConfigUpdate::ProjectMcpServersChanged { cwd: ref c } if *c == cwd),
            "first project event must dispatch"
        );

        // Second event, identical bytes → must be suppressed.
        event_tx.send(evt()).unwrap();
        let res = tokio::time::timeout(std::time::Duration::from_millis(400), rx.recv()).await;
        assert!(
            res.is_err(),
            "unchanged project config must not re-dispatch a reload"
        );

        // Real content change → dispatch again.
        std::fs::write(
            &mcp_json,
            r#"{"mcpServers":{"x":{"url":"http://localhost"}}}"#,
        )
        .unwrap();
        event_tx.send(evt()).unwrap();
        let update = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("changed content should dispatch within 2s")
            .expect("channel open");
        assert!(
            matches!(update, ConfigUpdate::ProjectMcpServersChanged { cwd: ref c } if *c == cwd),
            "changed project config must dispatch"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    /// `hash_project_mcp_config` is stable for identical content and
    /// changes on create/edit.
    #[test]
    fn hash_project_mcp_config_detects_create_and_change() {
        let tmp = tempfile::TempDir::new().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let cwd = tmp.path();

        let empty = hash_project_mcp_config(cwd).expect("readable");
        assert_eq!(empty, hash_project_mcp_config(cwd).expect("stable"));

        std::fs::write(cwd.join(".mcp.json"), "a").unwrap();
        let created = hash_project_mcp_config(cwd).expect("readable");
        assert_ne!(empty, created, "creating a config file changes the hash");

        std::fs::write(cwd.join(".mcp.json"), "b").unwrap();
        let changed = hash_project_mcp_config(cwd).expect("readable");
        assert_ne!(created, changed, "editing content changes the hash");
    }

    /// The hash must reflect ancestor `.kigi/config.toml` and `.mcp.json`
    /// under `cwd` — otherwise an ancestor edit would be wrongly suppressed.
    #[test]
    fn hash_project_mcp_config_covers_ancestors() {
        let tmp = tempfile::TempDir::new().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let child = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&child).unwrap();

        let h0 = hash_project_mcp_config(&child).expect("readable");

        std::fs::write(tmp.path().join(".mcp.json"), "a").unwrap();
        let h1 = hash_project_mcp_config(&child).expect("readable");
        assert_ne!(h0, h1, "ancestor .mcp.json create must change the hash");

        std::fs::write(tmp.path().join(".mcp.json"), "b").unwrap();
        let h2 = hash_project_mcp_config(&child).expect("readable");
        assert_ne!(h1, h2, "ancestor .mcp.json edit must change the hash");

        std::fs::create_dir_all(tmp.path().join(".kigi")).unwrap();
        std::fs::write(tmp.path().join(".kigi").join("config.toml"), "x = 1").unwrap();
        let h3 = hash_project_mcp_config(&child).expect("readable");
        assert_ne!(
            h2, h3,
            "ancestor .kigi/config.toml create must change the hash"
        );
    }

    #[test]
    fn parse_skills_config_empty() {
        let config = toml::Value::Table(toml::map::Map::new());
        let skills = parse_skills_config(&config);
        assert_eq!(skills, kigi_agent::prompt::skills::SkillsConfig::default());
    }

    #[test]
    fn parse_skills_config_with_paths() {
        let config: toml::Value = toml::from_str(
            r#"
[skills]
paths = ["/home/user/.kigi/skills"]
ignore = ["/tmp"]
"#,
        )
        .unwrap();
        let skills = parse_skills_config(&config);
        assert_eq!(skills.paths, vec!["/home/user/.kigi/skills".to_string()]);
        assert_eq!(skills.ignore, vec!["/tmp".to_string()]);
    }

    #[test]
    fn memory_config_diff_detects_enabled_change() {
        let empty = toml::Value::Table(toml::map::Map::new());
        let enabled: toml::Value = toml::from_str("[memory]\nenabled = true").unwrap();

        let old = crate::config::MemoryConfig::resolve(false, false, &empty, None);
        let new = crate::config::MemoryConfig::resolve(false, false, &enabled, None);
        assert_ne!(old, new, "should detect enabled field change");
    }

    #[test]
    fn memory_config_diff_detects_search_param_change() {
        let a: toml::Value = toml::from_str("[memory.search]\nmax_results = 6").unwrap();
        let b: toml::Value = toml::from_str("[memory.search]\nmax_results = 10").unwrap();

        let old = crate::config::MemoryConfig::resolve(false, false, &a, None);
        let new = crate::config::MemoryConfig::resolve(false, false, &b, None);
        assert_ne!(old, new, "should detect search param change");
    }

    #[test]
    fn extract_ui_fields_empty() {
        let config = toml::Value::Table(toml::map::Map::new());
        let (theme, yolo, fork) = extract_ui_fields(&config);
        assert_eq!(theme, None);
        assert!(!yolo);
        assert_eq!(fork, None);
    }

    #[test]
    fn extract_ui_fields_with_values() {
        let config: toml::Value = toml::from_str(
            r#"
[ui]
theme = "dark"
yolo = true
fork_secondary_model = "kigi-4.5"
"#,
        )
        .unwrap();
        let (theme, yolo, fork) = extract_ui_fields(&config);
        assert_eq!(theme.as_deref(), Some("dark"));
        assert!(yolo);
        assert_eq!(fork.as_deref(), Some("kigi-4.5"));
    }

    #[test]
    fn extract_ui_fields_diff_detects_theme_change() {
        let a: toml::Value = toml::from_str("[ui]\ntheme = \"light\"").unwrap();
        let b: toml::Value = toml::from_str("[ui]\ntheme = \"dark\"").unwrap();
        assert_ne!(extract_ui_fields(&a), extract_ui_fields(&b));
    }

    #[test]
    fn extract_ui_fields_diff_detects_yolo_change() {
        let a: toml::Value = toml::from_str("[ui]\nyolo = false").unwrap();
        let b: toml::Value = toml::from_str("[ui]\nyolo = true").unwrap();
        assert_ne!(extract_ui_fields(&a), extract_ui_fields(&b));
    }

    #[test]
    fn models_changed_detects_new_byok_model() {
        let a = toml::Value::Table(toml::map::Map::new());
        let b: toml::Value = toml::from_str(
            r#"
[model.my-custom]
model = "kigi-4.5"
base_url = "https://api.example.com/v1"
"#,
        )
        .unwrap();
        assert_ne!(a.get("model"), b.get("model"));
    }

    #[test]
    fn models_changed_detects_default_change() {
        let a: toml::Value = toml::from_str("[models]\ndefault = \"kigi-code-fast-1\"").unwrap();
        let b: toml::Value = toml::from_str("[models]\ndefault = \"kigi-code-slow-1\"").unwrap();
        assert_ne!(a.get("models"), b.get("models"));
    }

    #[test]
    fn mcp_servers_changed_detects_new_server() {
        let a = toml::Value::Table(toml::map::Map::new());
        let b: toml::Value = toml::from_str(
            r#"
[mcp_servers.test]
command = "/bin/test"
"#,
        )
        .unwrap();
        assert_ne!(a.get("mcp_servers"), b.get("mcp_servers"));
    }

    /// `ConfigUpdate::ProjectMcpServersChanged { cwd }` must be a distinct
    /// variant from the unit `McpServersChanged` so the two paths route
    /// through different match arms in `app.rs`. Guards against an accidental
    /// merge that would force per-cwd reloads through the sweep-all-sessions arm.
    #[test]
    fn project_variant_dispatches_separately() {
        let cwd = PathBuf::from("/tmp/proj-x");
        let global: ConfigUpdate = ConfigUpdate::McpServersChanged;
        let project = ConfigUpdate::ProjectMcpServersChanged { cwd: cwd.clone() };

        // Each variant must be matched by its own arm — fall-through
        // would indicate a single arm handling both.
        let mut routed_global = false;
        let mut routed_project = None;
        for u in [global, project] {
            match u {
                ConfigUpdate::McpServersChanged => routed_global = true,
                ConfigUpdate::ProjectMcpServersChanged { cwd } => routed_project = Some(cwd),
                _ => panic!("unexpected variant"),
            }
        }
        assert!(
            routed_global,
            "global variant must route through its own arm"
        );
        assert_eq!(routed_project.as_deref(), Some(cwd.as_path()));
    }

    /// `HomeClaudeJsonChanged` is **not** part of the per-cwd
    /// `collect_project_cwds` (otherwise sessions outside `$HOME` would be
    /// silently skipped). The reloader broadcasts it via the unit
    /// `McpServersChanged` variant; this test locks that invariant at the
    /// helper layer.
    #[test]
    fn collect_project_cwds_excludes_home_claude_json() {
        let batch = vec![
            ConfigChangeEvent::HomeClaudeJsonChanged,
            ConfigChangeEvent::ProjectConfigChanged {
                path: PathBuf::from("/repo/x/.kigi/config.toml"),
            },
        ];
        let cwds = collect_project_cwds(&batch);
        // Only the project entry contributes; the home-level `.claude.json`
        // entry is silently dropped because it routes through the
        // broadcast arm instead.
        assert_eq!(cwds, vec![PathBuf::from("/repo/x")]);
    }

    /// `collect_project_cwds` extracts `<cwd>` from `ProjectConfigChanged`
    /// (`<cwd>/.kigi/config.toml`) and `McpConfigChanged` (`<cwd>/.mcp.json`),
    /// de-duplicates while preserving order.
    #[test]
    fn collect_project_cwds_dedupes_and_extracts() {
        let batch = vec![
            ConfigChangeEvent::ProjectConfigChanged {
                path: PathBuf::from("/repo/a/.kigi/config.toml"),
            },
            ConfigChangeEvent::McpConfigChanged {
                path: PathBuf::from("/repo/a/.mcp.json"),
            },
            ConfigChangeEvent::ProjectConfigChanged {
                path: PathBuf::from("/repo/b/.kigi/config.toml"),
            },
        ];
        let cwds = collect_project_cwds(&batch);
        assert_eq!(
            cwds,
            vec![PathBuf::from("/repo/a"), PathBuf::from("/repo/b")]
        );
    }

    #[test]
    fn mcp_servers_unchanged_same_config() {
        let cfg: toml::Value = toml::from_str(
            r#"
[mcp_servers.test]
command = "/bin/test"
"#,
        )
        .unwrap();
        assert_eq!(cfg.get("mcp_servers"), cfg.get("mcp_servers"));
    }
}
