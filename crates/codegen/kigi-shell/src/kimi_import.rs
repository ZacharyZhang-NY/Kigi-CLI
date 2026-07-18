// kimi_import.rs
// One-time, READ-ONLY import of the official kimi-cli configuration (PRD F7).
//
// Sources (shapes ported from kimi-cli 1.49.0):
// - `~/.kimi/config.toml`: top-level `default_model` plus `[models.<alias>]`
//   (`src/kimi_cli/config.py::LLMModel { provider, model, max_context_size,
//   capabilities }`) and `[providers.<name>]`
//   (`src/kimi_cli/config.py::LLMProvider { type, base_url, api_key, env,
//   custom_headers }`).
// - `~/.kimi/mcp.json`: `{"mcpServers": {...}}` — the exact shape kigi
//   already parses via [`McpConfig`].
//
// READ-ONLY invariant: nothing under the kimi dir is ever written, modified,
// or deleted — the scanner opens both files with plain `fs::read_to_string`
// (contents and mtimes stay untouched) and `apply_*` receives an owned plan,
// writing exclusively into the kigi home. Keyring credentials are NOT
// imported (kigi has its own login). `KIMI_SHARE_DIR` (or any `KIMI_*` env
// var) is never consulted — the official dir is hardcoded to `~/.kimi`.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Deserialize;
use toml::Value as TomlValue;
use toml::map::Map as TomlMap;
use tracing::{debug, info};

use crate::util::config::{McpConfig, McpServerConfig, McpServerTransportConfig};
use kigi_models::PlatformId;

// Types

/// A kimi-cli model entry that maps to a kigi `[model.<alias>]` custom entry
/// (non-built-in provider).
#[derive(Debug, Clone)]
pub struct KimiCustomModel {
    /// kimi `[models.<alias>]` key; becomes the kigi `[model.<alias>]` key.
    pub alias: String,
    /// Provider-side model id (`LLMModel.model`).
    pub model: String,
    /// `LLMProvider.base_url` of the model's provider.
    pub base_url: String,
    /// `LLMProvider.api_key`. SECRET: flows only into the user's own
    /// config.toml (same trust domain); never logged or shown in summaries.
    pub api_key: Option<String>,
    /// `LLMModel.max_context_size` → kigi `context_window`.
    pub context_window: Option<u64>,
}

/// What a scan of `~/.kimi` found to import.
#[derive(Debug, Clone, Default)]
pub struct KimiImportPlan {
    /// MCP servers from `~/.kimi/mcp.json`, in file order.
    pub mcp_servers: Vec<(String, McpServerConfig)>,
    /// Models on non-built-in providers, imported as `[model.<alias>]`.
    pub custom_models: Vec<KimiCustomModel>,
    /// The kigi `[models] default` value the kimi `default_model` maps to:
    /// the alias itself for an imported custom model, or the managed catalog
    /// key (`{platform_id}/{model_id}`) for a built-in-platform model.
    pub default_model: Option<String>,
    /// Models skipped because kigi's built-in platform registry already
    /// covers their provider: `(alias, managed catalog key)`.
    pub skipped_builtin: Vec<(String, String)>,
    /// Non-fatal caveats surfaced in the summary.
    pub notes: Vec<String>,
}

impl KimiImportPlan {
    /// Whether there is nothing to import. Built-in-platform models alone
    /// don't count — kigi already ships those platforms.
    pub fn is_empty(&self) -> bool {
        self.mcp_servers.is_empty() && self.custom_models.is_empty() && self.default_model.is_none()
    }

    /// Human-readable summary. Never includes api_key values.
    pub fn summary(&self) -> String {
        let mut out = String::from("Found kimi-cli settings to import from ~/.kimi:\n");
        if !self.mcp_servers.is_empty() {
            out.push_str(&format!("  - {} MCP server(s)\n", self.mcp_servers.len()));
            for (name, config) in &self.mcp_servers {
                let target = match &config.transport {
                    McpServerTransportConfig::Stdio { command, args, .. } => {
                        if args.is_empty() {
                            command.clone()
                        } else {
                            format!("{} {}", command, args.join(" "))
                        }
                    }
                    McpServerTransportConfig::StreamableHttp { url, .. } => url.clone(),
                };
                out.push_str(&format!("      {name}: {target}\n"));
            }
        }
        if !self.custom_models.is_empty() {
            out.push_str(&format!(
                "  - {} custom model(s)\n",
                self.custom_models.len()
            ));
            for m in &self.custom_models {
                // Redact the key: only its presence and length are shown, so
                // the secret never reaches terminals, screenshots, or logs.
                let key_note = match &m.api_key {
                    Some(k) => format!(" (api key: <redacted, {} chars>)", k.len()),
                    None => String::new(),
                };
                out.push_str(&format!(
                    "      {}: {} @ {}{}\n",
                    m.alias, m.model, m.base_url, key_note
                ));
            }
        }
        if let Some(default) = &self.default_model {
            out.push_str(&format!("  - default model: {default}\n"));
        }
        if !self.skipped_builtin.is_empty() {
            out.push_str("  Skipped (kigi already has these platforms built in):\n");
            for (alias, managed_key) in &self.skipped_builtin {
                out.push_str(&format!("      {alias} -> {managed_key}\n"));
            }
        }
        for note in &self.notes {
            out.push_str(&format!("  Note: {note}\n"));
        }
        out
    }
}

// kimi-cli config.toml wire shapes (subset we import)

#[derive(Debug, Deserialize)]
struct KimiConfigToml {
    #[serde(default)]
    default_model: String,
    #[serde(default)]
    models: IndexMap<String, KimiModelToml>,
    #[serde(default)]
    providers: IndexMap<String, KimiProviderToml>,
}

/// `src/kimi_cli/config.py::LLMModel` (fields we import).
#[derive(Debug, Deserialize)]
struct KimiModelToml {
    provider: String,
    model: String,
    /// Required in kimi-cli; tolerated as absent here because kigi has its
    /// own context-window default.
    #[serde(default)]
    max_context_size: Option<u64>,
}

/// `src/kimi_cli/config.py::LLMProvider` (fields we import).
#[derive(Debug, Deserialize)]
struct KimiProviderToml {
    #[serde(rename = "type")]
    provider_type: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: Option<String>,
}

/// Built-in kigi platform a kimi provider duplicates, if any: provider type
/// `kimi` is the Kimi Code subscription channel; the two Moonshot open
/// platforms are recognized by their fixed production hosts (the same hosts
/// `kigi_models::PlatformId::base_url` compiles in).
fn builtin_platform(provider: &KimiProviderToml) -> Option<PlatformId> {
    if provider.provider_type == "kimi" {
        return Some(PlatformId::KimiCode);
    }
    match url_host(&provider.base_url) {
        Some("api.moonshot.cn") => Some(PlatformId::MoonshotCn),
        Some("api.moonshot.ai") => Some(PlatformId::MoonshotAi),
        _ => None,
    }
}

/// Host component of an http(s) URL. `None` for other schemes.
fn url_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let end = rest.find(['/', ':', '?']).unwrap_or(rest.len());
    Some(&rest[..end])
}

// Scanner

/// The official kimi-cli share dir. Hardcoded — `KIMI_SHARE_DIR` is
/// deliberately NOT consulted (PRD F7).
pub fn default_kimi_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".kimi"))
}

/// Scan `~/.kimi` for importable settings.
///
/// Returns `Ok(None)` when the one-time marker is already set, when
/// `~/.kimi/config.toml` is absent, or when the plan would be empty.
/// Malformed files are an error (surfaced by `kigi import-kimi`), not a
/// silent "nothing to import".
pub fn scan() -> anyhow::Result<Option<KimiImportPlan>> {
    if is_kimi_import_marked() {
        return Ok(None);
    }
    let Some(kimi_dir) = default_kimi_dir() else {
        return Ok(None);
    };
    scan_kimi_dir(&kimi_dir)
}

/// Testable variant of [`scan`]: marker checked at an explicit path, kimi dir
/// passed in. Never touches the real home.
pub fn scan_with_marker(
    kimi_dir: &Path,
    marker_path: &Path,
) -> anyhow::Result<Option<KimiImportPlan>> {
    if marker_path.exists() {
        return Ok(None);
    }
    scan_kimi_dir(kimi_dir)
}

/// Read `<kimi_dir>/config.toml` + `<kimi_dir>/mcp.json` (read-only) and
/// build the plan. `Ok(None)` when config.toml is absent or the plan is empty.
fn scan_kimi_dir(kimi_dir: &Path) -> anyhow::Result<Option<KimiImportPlan>> {
    let config_path = kimi_dir.join("config.toml");
    let raw = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "cannot read {}: {e}",
                config_path.display()
            ));
        }
    };
    let config: KimiConfigToml = toml::from_str(&raw).map_err(|e| {
        anyhow::anyhow!(
            "{} is not a valid kimi-cli config ({e}). Fix the file and re-run \
             `kigi import-kimi`.",
            config_path.display()
        )
    })?;

    let mut plan = KimiImportPlan::default();

    for (alias, model) in &config.models {
        let Some(provider) = config.providers.get(&model.provider) else {
            return Err(anyhow::anyhow!(
                "{}: model '{alias}' references provider '{}' which has no \
                 [providers.{}] entry. Fix the file and re-run `kigi import-kimi`.",
                config_path.display(),
                model.provider,
                model.provider
            ));
        };
        if let Some(platform) = builtin_platform(provider) {
            plan.skipped_builtin
                .push((alias.clone(), platform.managed_model_key(&model.model)));
            continue;
        }
        if provider.base_url.is_empty() {
            return Err(anyhow::anyhow!(
                "{}: provider '{}' has an empty base_url. Fix the file and \
                 re-run `kigi import-kimi`.",
                config_path.display(),
                model.provider
            ));
        }
        plan.custom_models.push(KimiCustomModel {
            alias: alias.clone(),
            model: model.model.clone(),
            base_url: provider.base_url.clone(),
            api_key: provider.api_key.clone().filter(|k| !k.is_empty()),
            context_window: model.max_context_size,
        });
    }

    if !config.default_model.is_empty() {
        let alias = &config.default_model;
        if plan.custom_models.iter().any(|m| &m.alias == alias) {
            plan.default_model = Some(alias.clone());
        } else if let Some((_, managed_key)) = plan.skipped_builtin.iter().find(|(a, _)| a == alias)
        {
            // kigi refers to built-in platform models by their managed
            // catalog key ({platform_id}/{model_id}).
            plan.default_model = Some(managed_key.clone());
        } else {
            // kimi-cli validates default_model against [models], so this only
            // happens with a hand-edited file. Not worth failing the import.
            plan.notes.push(format!(
                "default_model '{alias}' has no [models.{alias}] entry; \
                 default model preference not imported"
            ));
        }
    }

    let mcp_path = kimi_dir.join("mcp.json");
    match std::fs::read_to_string(&mcp_path) {
        Ok(s) => {
            let mcp: McpConfig = serde_json::from_str(&s).map_err(|e| {
                anyhow::anyhow!(
                    "{} is not valid MCP JSON ({e}). Fix the file and re-run \
                     `kigi import-kimi`.",
                    mcp_path.display()
                )
            })?;
            plan.mcp_servers = mcp.mcp_servers.into_iter().collect();
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::anyhow!("cannot read {}: {e}", mcp_path.display())),
    }

    if plan.is_empty() {
        debug!(
            "kimi import scan: nothing importable in {}",
            kimi_dir.display()
        );
        return Ok(None);
    }
    info!(
        mcp = plan.mcp_servers.len(),
        custom_models = plan.custom_models.len(),
        skipped_builtin = plan.skipped_builtin.len(),
        default_model = plan.default_model.is_some(),
        "Scanned kimi-cli settings for import"
    );
    Ok(Some(plan))
}

/// Display-only helper for the welcome-screen hint: true when a scan would
/// offer something. Scan errors (unreadable or malformed `~/.kimi` files) are
/// reported as `false` with a debug log — `kigi import-kimi` is the surface
/// that explains failures; the startup hint must never block launch.
pub fn has_pending_kimi_import() -> bool {
    match scan() {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            debug!(error = %e, "kimi import scan failed; hiding startup hint");
            false
        }
    }
}

// Applier

/// Result of applying a [`KimiImportPlan`].
#[derive(Debug, Default)]
pub struct KimiApplied {
    /// The kigi config file that received the entries.
    pub config_path: PathBuf,
    /// The one-time marker file written under the kigi home.
    pub marker_path: PathBuf,
    pub mcp_added: Vec<String>,
    /// Names already present in kigi config, left untouched (never clobber).
    pub mcp_skipped_existing: Vec<String>,
    pub models_added: Vec<String>,
    /// Aliases already present as `[model.<alias>]`, left untouched.
    pub models_skipped_existing: Vec<String>,
    /// The `[models] default` value written, if any.
    pub default_model_set: Option<String>,
    /// True when `[models] default` was already set and left untouched.
    pub default_model_kept_existing: bool,
}

impl KimiApplied {
    pub fn total_added(&self) -> usize {
        self.mcp_added.len()
            + self.models_added.len()
            + usize::from(self.default_model_set.is_some())
    }

    /// Human-readable result summary.
    pub fn summary(&self) -> String {
        let mut out = format!("Imported into {}:\n", self.config_path.display());
        if !self.mcp_added.is_empty() {
            out.push_str(&format!(
                "  - added MCP server(s): {}\n",
                self.mcp_added.join(", ")
            ));
        }
        if !self.mcp_skipped_existing.is_empty() {
            out.push_str(&format!(
                "  - kept existing MCP server(s) (not overwritten): {}\n",
                self.mcp_skipped_existing.join(", ")
            ));
        }
        if !self.models_added.is_empty() {
            out.push_str(&format!(
                "  - added model(s): {}\n",
                self.models_added.join(", ")
            ));
        }
        if !self.models_skipped_existing.is_empty() {
            out.push_str(&format!(
                "  - kept existing model(s) (not overwritten): {}\n",
                self.models_skipped_existing.join(", ")
            ));
        }
        if let Some(default) = &self.default_model_set {
            out.push_str(&format!("  - set default model: {default}\n"));
        }
        if self.default_model_kept_existing {
            out.push_str("  - kept existing default model (not overwritten)\n");
        }
        if self.total_added() == 0 {
            out.push_str("  - nothing new to add\n");
        }
        out.push_str(&format!(
            "One-time import complete; marker written to {}.\n",
            self.marker_path.display()
        ));
        out
    }
}

/// Apply the plan to the user's kigi home and set the one-time marker.
pub fn apply(plan: &KimiImportPlan) -> anyhow::Result<KimiApplied> {
    let applied = apply_at(plan, &crate::util::kigi_home::kigi_home())?;
    refresh_kimi_marker_cache(true);
    Ok(applied)
}

/// Testable variant of [`apply`]: writes ONLY under `kigi_home` (config.toml
/// merge + marker file). Existing entries are never overwritten.
pub fn apply_at(plan: &KimiImportPlan, kigi_home: &Path) -> anyhow::Result<KimiApplied> {
    let config_path = kigi_home.join("config.toml");
    // Surface parse errors instead of silently discarding the file — an
    // atomic rewrite would otherwise drop unrelated sections (same policy as
    // claude_import::apply_items_to_config).
    let mut root: TomlValue = match std::fs::read_to_string(&config_path) {
        Ok(s) => toml::from_str(&s).map_err(|e| {
            anyhow::anyhow!(
                "refusing to import: existing config at {} is not valid TOML \
                 ({e}). Fix the file (or move it aside) and retry.",
                config_path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TomlValue::Table(TomlMap::new()),
        Err(e) => return Err(e.into()),
    };
    let table = root
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config root is not a table"))?;

    let mut applied = KimiApplied {
        config_path: config_path.clone(),
        marker_path: kigi_home.join(KIMI_IMPORT_MARKER_FILE),
        ..KimiApplied::default()
    };

    if !plan.mcp_servers.is_empty() {
        let servers = table
            .entry("mcp_servers")
            .or_insert_with(|| TomlValue::Table(TomlMap::new()))
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("[mcp_servers] is not a table"))?;
        for (name, config) in &plan.mcp_servers {
            if servers.contains_key(name) {
                applied.mcp_skipped_existing.push(name.clone());
                continue;
            }
            let serialized = TomlValue::try_from(config)
                .map_err(|e| anyhow::anyhow!("failed to serialize MCP server {name}: {e}"))?;
            servers.insert(name.clone(), serialized);
            applied.mcp_added.push(name.clone());
        }
    }

    if !plan.custom_models.is_empty() {
        let models = table
            .entry("model")
            .or_insert_with(|| TomlValue::Table(TomlMap::new()))
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("[model] is not a table"))?;
        for m in &plan.custom_models {
            if models.contains_key(&m.alias) {
                applied.models_skipped_existing.push(m.alias.clone());
                continue;
            }
            let mut entry = TomlMap::new();
            entry.insert("model".to_string(), TomlValue::String(m.model.clone()));
            entry.insert(
                "base_url".to_string(),
                TomlValue::String(m.base_url.clone()),
            );
            if let Some(api_key) = &m.api_key {
                entry.insert("api_key".to_string(), TomlValue::String(api_key.clone()));
            }
            if let Some(cw) = m.context_window {
                let cw = i64::try_from(cw).map_err(|_| {
                    anyhow::anyhow!(
                        "model '{}': max_context_size {cw} does not fit a TOML integer",
                        m.alias
                    )
                })?;
                entry.insert("context_window".to_string(), TomlValue::Integer(cw));
            }
            models.insert(m.alias.clone(), TomlValue::Table(entry));
            applied.models_added.push(m.alias.clone());
        }
    }

    if let Some(default) = &plan.default_model {
        let models_section = table
            .entry("models")
            .or_insert_with(|| TomlValue::Table(TomlMap::new()))
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("[models] is not a table"))?;
        if models_section.contains_key("default") {
            applied.default_model_kept_existing = true;
        } else {
            models_section.insert("default".to_string(), TomlValue::String(default.clone()));
            applied.default_model_set = Some(default.clone());
        }
    }

    std::fs::create_dir_all(kigi_home)?;
    if applied.total_added() > 0 {
        // Atomic write: tmp + rename (same pattern as save_mcp_server_config).
        let toml_str = toml::to_string_pretty(&root)?;
        let tmp = config_path.with_extension("toml.tmp");
        if let Err(e) = std::fs::write(&tmp, &toml_str) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        if let Err(e) = std::fs::rename(&tmp, &config_path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        info!(
            path = %config_path.display(),
            added = applied.total_added(),
            "Wrote imported kimi-cli settings to config.toml"
        );
    }

    // The marker is written even when nothing new was added: the import is
    // the user's one-time choice, and re-offering it every launch after an
    // all-collisions apply would be noise.
    std::fs::write(
        &applied.marker_path,
        format!("imported {}\n", chrono::Utc::now().to_rfc3339()),
    )?;

    Ok(applied)
}

// One-Time Marker

/// Marker file under the kigi home recording that the one-time kimi import
/// ran (successfully applied OR found nothing). Presence = done.
const KIMI_IMPORT_MARKER_FILE: &str = "kimi_import_done";

/// Path of the marker under the user's kigi home.
pub fn kimi_import_marker_path() -> PathBuf {
    crate::util::kigi_home::kigi_home().join(KIMI_IMPORT_MARKER_FILE)
}

/// Cached marker state, mirroring `claude_import::MARKER_CACHE`:
/// `RwLock<Option<bool>>` so tests could reset it, one stat per process.
static KIMI_MARKER_CACHE: std::sync::RwLock<Option<bool>> = std::sync::RwLock::new(None);

/// Whether the one-time kimi import already ran for this user. Checked by
/// [`scan`] and the TUI startup hint; cached per process (a user who removes
/// the marker mid-session must restart, same trade-off as the claude marker).
pub fn is_kimi_import_marked() -> bool {
    if let Some(v) = *KIMI_MARKER_CACHE
        .read()
        .expect("KIMI_MARKER_CACHE poisoned")
    {
        return v;
    }
    let v = kimi_import_marker_path().exists();
    *KIMI_MARKER_CACHE
        .write()
        .expect("KIMI_MARKER_CACHE poisoned") = Some(v);
    v
}

/// Seed the cache after [`apply`] writes the marker so in-process checks
/// reflect the new state without restart.
pub fn refresh_kimi_marker_cache(value: bool) {
    *KIMI_MARKER_CACHE
        .write()
        .expect("KIMI_MARKER_CACHE poisoned") = Some(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic fake `~/.kimi` with two MCP servers, one built-in (kimi)
    /// provider, one custom (openai-compatible) provider, and a default model
    /// pointing at the custom alias. Server names are namespaced so loader
    /// assertions can't collide with entries from a developer's real config.
    fn write_fake_kimi_dir(kimi_dir: &Path) {
        std::fs::create_dir_all(kimi_dir).unwrap();
        std::fs::write(
            kimi_dir.join("config.toml"),
            r#"
default_model = "my-openai"

[models.k2]
provider = "kimi"
model = "kimi-for-coding"
max_context_size = 262144

[models.my-openai]
provider = "openrouter"
model = "gpt-x"
max_context_size = 128000

[providers.kimi]
type = "kimi"
base_url = "https://api.kimi.com/coding/v1"
api_key = "sk-kimi-secret"

[providers.openrouter]
type = "openai_legacy"
base_url = "https://openrouter.ai/api/v1"
api_key = "sk-or-secret"
"#,
        )
        .unwrap();
        std::fs::write(
            kimi_dir.join("mcp.json"),
            r#"{
  "mcpServers": {
    "kimi-import-test-fs": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem"]
    },
    "kimi-import-test-linear": {
      "url": "https://mcp.linear.app/sse"
    }
  }
}"#,
        )
        .unwrap();
    }

    /// `(content, mtime)` snapshot of every file directly under `dir`.
    fn snapshot_dir(dir: &Path) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        entries
            .into_iter()
            .map(|p| {
                let content = std::fs::read(&p).unwrap();
                let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
                (p, content, mtime)
            })
            .collect()
    }

    fn scan_fake(kimi_dir: &Path, marker: &Path) -> KimiImportPlan {
        scan_with_marker(kimi_dir, marker)
            .expect("scan must succeed")
            .expect("plan must be non-empty")
    }

    #[test]
    fn scan_parses_mcp_servers_models_and_default() {
        let tmp = tempfile::tempdir().unwrap();
        let kimi_dir = tmp.path().join("kimi");
        write_fake_kimi_dir(&kimi_dir);

        let plan = scan_fake(&kimi_dir, &tmp.path().join("no-marker"));

        assert_eq!(plan.mcp_servers.len(), 2);
        let (fs_name, fs_cfg) = plan
            .mcp_servers
            .iter()
            .find(|(n, _)| n == "kimi-import-test-fs")
            .expect("fs server in plan");
        assert_eq!(fs_name, "kimi-import-test-fs");
        match &fs_cfg.transport {
            McpServerTransportConfig::Stdio { command, args, .. } => {
                assert_eq!(command, "npx");
                assert_eq!(
                    args,
                    &[
                        "-y".to_string(),
                        "@modelcontextprotocol/server-filesystem".to_string()
                    ]
                );
            }
            other => panic!("expected stdio transport, got {other:?}"),
        }
        let (_, linear_cfg) = plan
            .mcp_servers
            .iter()
            .find(|(n, _)| n == "kimi-import-test-linear")
            .expect("linear server in plan");
        match &linear_cfg.transport {
            McpServerTransportConfig::StreamableHttp { url, .. } => {
                assert_eq!(url, "https://mcp.linear.app/sse");
            }
            other => panic!("expected http transport, got {other:?}"),
        }

        assert_eq!(plan.custom_models.len(), 1);
        let m = &plan.custom_models[0];
        assert_eq!(m.alias, "my-openai");
        assert_eq!(m.model, "gpt-x");
        assert_eq!(m.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(m.api_key.as_deref(), Some("sk-or-secret"));
        assert_eq!(m.context_window, Some(128_000));

        // The kimi-platform model is skipped, mapped to its managed key.
        assert_eq!(
            plan.skipped_builtin,
            vec![("k2".to_string(), "kimi-code/kimi-for-coding".to_string())]
        );
        assert_eq!(plan.default_model.as_deref(), Some("my-openai"));

        // Secrets never surface in the human-facing summary.
        let summary = plan.summary();
        assert!(!summary.contains("sk-or-secret"), "summary leaked api key");
        assert!(summary.contains("my-openai"));
        assert!(summary.contains("kimi-import-test-fs"));
    }

    #[test]
    fn scan_maps_builtin_default_to_managed_key() {
        let tmp = tempfile::tempdir().unwrap();
        let kimi_dir = tmp.path().join("kimi");
        std::fs::create_dir_all(&kimi_dir).unwrap();
        // Default points at a moonshot open-platform model (recognized by
        // host, not provider type).
        std::fs::write(
            kimi_dir.join("config.toml"),
            r#"
default_model = "k2t"

[models.k2t]
provider = "moonshot"
model = "kimi-k2-turbo-preview"
max_context_size = 262144

[providers.moonshot]
type = "openai_legacy"
base_url = "https://api.moonshot.cn/v1"
api_key = "sk-ms"
"#,
        )
        .unwrap();

        let plan = scan_fake(&kimi_dir, &tmp.path().join("no-marker"));
        assert!(plan.custom_models.is_empty());
        assert_eq!(
            plan.default_model.as_deref(),
            Some("moonshot-cn/kimi-k2-turbo-preview")
        );
    }

    #[test]
    fn scan_returns_none_when_config_absent_or_plan_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let kimi_dir = tmp.path().join("kimi");
        let marker = tmp.path().join("no-marker");

        // No ~/.kimi/config.toml at all (even with an mcp.json present:
        // detection is keyed on config.toml per PRD F7).
        std::fs::create_dir_all(&kimi_dir).unwrap();
        assert!(scan_with_marker(&kimi_dir, &marker).unwrap().is_none());

        // config.toml exists but has nothing importable.
        std::fs::write(kimi_dir.join("config.toml"), "theme = \"dark\"\n").unwrap();
        assert!(scan_with_marker(&kimi_dir, &marker).unwrap().is_none());
    }

    #[test]
    fn scan_fails_fast_on_malformed_files() {
        let tmp = tempfile::tempdir().unwrap();
        let kimi_dir = tmp.path().join("kimi");
        let marker = tmp.path().join("no-marker");
        std::fs::create_dir_all(&kimi_dir).unwrap();

        std::fs::write(kimi_dir.join("config.toml"), "default_model = [not toml").unwrap();
        let err = scan_with_marker(&kimi_dir, &marker).expect_err("malformed toml must fail");
        assert!(err.to_string().contains("config.toml"), "got: {err}");

        // Model referencing an undefined provider is corrupt, not skippable.
        std::fs::write(
            kimi_dir.join("config.toml"),
            "[models.m]\nprovider = \"ghost\"\nmodel = \"x\"\nmax_context_size = 1\n",
        )
        .unwrap();
        let err = scan_with_marker(&kimi_dir, &marker).expect_err("dangling provider must fail");
        assert!(err.to_string().contains("ghost"), "got: {err}");

        // Malformed mcp.json fails even when config.toml is fine.
        std::fs::write(
            kimi_dir.join("config.toml"),
            "[models.m]\nprovider = \"p\"\nmodel = \"x\"\nmax_context_size = 1\n\
             [providers.p]\ntype = \"openai_legacy\"\nbase_url = \"https://x.example/v1\"\n",
        )
        .unwrap();
        std::fs::write(kimi_dir.join("mcp.json"), "{ not json").unwrap();
        let err = scan_with_marker(&kimi_dir, &marker).expect_err("malformed json must fail");
        assert!(err.to_string().contains("mcp.json"), "got: {err}");
    }

    #[test]
    fn apply_writes_kigi_config_and_loader_sees_both_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let kimi_dir = tmp.path().join("kimi");
        write_fake_kimi_dir(&kimi_dir);
        // Use `<project>/.kigi` as the kigi home so the standard
        // project-config loader can observe the result from disk.
        let project = tmp.path().join("project");
        let kigi_home = project.join(".kigi");

        let plan = scan_fake(&kimi_dir, &tmp.path().join("no-marker"));
        let applied = apply_at(&plan, &kigi_home).expect("apply must succeed");

        assert_eq!(
            applied.mcp_added,
            vec!["kimi-import-test-fs", "kimi-import-test-linear"]
        );
        assert_eq!(applied.models_added, vec!["my-openai"]);
        assert_eq!(applied.default_model_set.as_deref(), Some("my-openai"));
        assert!(applied.marker_path.is_file(), "marker file must be written");

        // Raw TOML shape: [mcp_servers.*], [model.*], [models] default.
        let written = std::fs::read_to_string(kigi_home.join("config.toml")).unwrap();
        let root: TomlValue = toml::from_str(&written).unwrap();
        assert_eq!(
            root["mcp_servers"]["kimi-import-test-fs"]["command"]
                .as_str()
                .unwrap(),
            "npx"
        );
        assert_eq!(
            root["mcp_servers"]["kimi-import-test-linear"]["url"]
                .as_str()
                .unwrap(),
            "https://mcp.linear.app/sse"
        );
        let model = &root["model"]["my-openai"];
        assert_eq!(model["model"].as_str().unwrap(), "gpt-x");
        assert_eq!(
            model["base_url"].as_str().unwrap(),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(model["api_key"].as_str().unwrap(), "sk-or-secret");
        assert_eq!(model["context_window"].as_integer().unwrap(), 128_000);
        assert_eq!(root["models"]["default"].as_str().unwrap(), "my-openai");

        // `kigi mcp list`-level loader (project overlay path) sees both.
        let servers = crate::util::config::load_mcp_server_configs_with_project(&project);
        for name in ["kimi-import-test-fs", "kimi-import-test-linear"] {
            let (_, scope) = servers
                .get(name)
                .unwrap_or_else(|| panic!("loader must see imported server {name}"));
            assert_eq!(*scope, "project");
        }
    }

    #[test]
    fn scan_after_apply_returns_none_via_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let kimi_dir = tmp.path().join("kimi");
        write_fake_kimi_dir(&kimi_dir);
        let kigi_home = tmp.path().join("kigi-home");
        let marker = kigi_home.join(KIMI_IMPORT_MARKER_FILE);

        let plan = scan_fake(&kimi_dir, &marker);
        apply_at(&plan, &kigi_home).unwrap();

        assert!(
            scan_with_marker(&kimi_dir, &marker).unwrap().is_none(),
            "second scan must be gated by the one-time marker"
        );
    }

    #[test]
    fn apply_never_touches_the_kimi_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let kimi_dir = tmp.path().join("kimi");
        write_fake_kimi_dir(&kimi_dir);
        let before = snapshot_dir(&kimi_dir);

        let plan = scan_fake(&kimi_dir, &tmp.path().join("no-marker"));
        apply_at(&plan, &tmp.path().join("kigi-home")).unwrap();

        let after = snapshot_dir(&kimi_dir);
        assert_eq!(
            before.len(),
            after.len(),
            "no files may appear or vanish under ~/.kimi"
        );
        for ((path_b, content_b, mtime_b), (path_a, content_a, mtime_a)) in
            before.iter().zip(after.iter())
        {
            assert_eq!(path_b, path_a);
            assert_eq!(
                content_b,
                content_a,
                "{}: content changed",
                path_b.display()
            );
            assert_eq!(mtime_b, mtime_a, "{}: mtime changed", path_b.display());
        }
    }

    #[test]
    fn apply_never_clobbers_existing_kigi_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let kimi_dir = tmp.path().join("kimi");
        write_fake_kimi_dir(&kimi_dir);
        let kigi_home = tmp.path().join("kigi-home");
        std::fs::create_dir_all(&kigi_home).unwrap();
        std::fs::write(
            kigi_home.join("config.toml"),
            r#"
[models]
default = "existing-default"

[mcp_servers.kimi-import-test-fs]
command = "original-command"

[model.my-openai]
model = "original-model"
"#,
        )
        .unwrap();

        let plan = scan_fake(&kimi_dir, &tmp.path().join("no-marker"));
        let applied = apply_at(&plan, &kigi_home).unwrap();

        assert_eq!(applied.mcp_added, vec!["kimi-import-test-linear"]);
        assert_eq!(applied.mcp_skipped_existing, vec!["kimi-import-test-fs"]);
        assert!(applied.models_added.is_empty());
        assert_eq!(applied.models_skipped_existing, vec!["my-openai"]);
        assert_eq!(applied.default_model_set, None);
        assert!(applied.default_model_kept_existing);

        let written = std::fs::read_to_string(kigi_home.join("config.toml")).unwrap();
        let root: TomlValue = toml::from_str(&written).unwrap();
        assert_eq!(
            root["mcp_servers"]["kimi-import-test-fs"]["command"]
                .as_str()
                .unwrap(),
            "original-command",
            "existing MCP server must not be overwritten"
        );
        assert_eq!(
            root["model"]["my-openai"]["model"].as_str().unwrap(),
            "original-model",
            "existing model entry must not be overwritten"
        );
        assert_eq!(
            root["models"]["default"].as_str().unwrap(),
            "existing-default",
            "existing default model must not be overwritten"
        );
        // The non-colliding server was still added.
        assert!(root["mcp_servers"].get("kimi-import-test-linear").is_some());
    }
}
