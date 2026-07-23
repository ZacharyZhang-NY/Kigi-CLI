use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use tokio::fs;

use kigi_shell::util::kigi_home::kigi_home;

const TTL_SECONDS_BEFORE_AUTO_UPDATE: Duration = Duration::from_secs(60 * 30);

/// Release channel: this repo's GitHub Releases (PRD F8). The API base is
/// resolved through [`kigi_env::update_base_url`] — production default
/// `https://api.github.com/repos/ZacharyZhang-NY/Kigi-CLI/releases`,
/// overridable via `KIGI_UPDATE_BASE_URL` for mirrors and tests.
pub(crate) fn update_base_url() -> String {
    kigi_env::update_base_url()
}

/// Minimal configuration the update system needs from the environment.
///
/// Constructed once at startup and threaded through the update call chain.
#[derive(Debug, Clone)]
pub struct UpdateConfig {
    /// Subscription coding-API base URL ([`kigi_env::coding_api_base_url`]).
    pub proxy_base_url: String,
    /// Auth scope key for `~/.kigi/auth.json`.
    pub auth_scope: String,
    /// Enterprise deployment key (KIGI_DEPLOYMENT_KEY).
    pub deployment_key: Option<String>,
    /// Optional extra auth material forwarded with requests when present.
    pub alpha_test_key: Option<String>,
    /// Release channel: "stable" or "alpha". Loaded from config.
    pub channel: String,
}

impl UpdateConfig {
    pub fn from_environment() -> Self {
        Self {
            proxy_base_url: kigi_env::coding_api_base_url(),
            auth_scope: kigi_shell::auth::KimiCodeConfig::default().auth_scope(),
            deployment_key: None,
            alpha_test_key: None,
            channel: "stable".to_string(),
        }
    }
}

// GitHub Releases API wire shape

/// One downloadable asset attached to a GitHub release.
///
/// Wire shape per the GitHub REST API
/// (<https://docs.github.com/en/rest/releases/releases#get-the-latest-release>):
/// `GET /repos/{owner}/{repo}/releases/latest` →
/// `{"tag_name":"v0.1.0","assets":[{"name":"...","browser_download_url":"..."}]}`.
/// Unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}

/// A GitHub release, reduced to the fields the updater consumes.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    /// Git tag, e.g. `v0.1.0`. [`Release::version`] strips the `v` prefix.
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

impl Release {
    /// Semver version from `tag_name` (`v0.1.0` → `0.1.0`). Errors on a tag
    /// that is not a `v`-prefixed (or bare) semver string.
    pub fn version(&self) -> Result<String> {
        let v = self.tag_name.strip_prefix('v').unwrap_or(&self.tag_name);
        semver::Version::parse(v)
            .map_err(|e| anyhow::anyhow!("release tag '{}' is not semver: {e}", self.tag_name))?;
        Ok(v.to_string())
    }

    /// Asset with exactly `name`, or an error listing what the release has.
    pub fn asset(&self, name: &str) -> Result<&ReleaseAsset> {
        self.assets.iter().find(|a| a.name == name).ok_or_else(|| {
            anyhow::anyhow!(
                "release {} has no asset named '{}' (available: {})",
                self.tag_name,
                name,
                self.assets
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    }
}

/// Shared HTTP client for GitHub API metadata requests. GitHub rejects
/// requests without a `User-Agent`, so one is always set.
fn github_api_client(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("kigi/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .build()?)
}

/// GET `url` and decode the JSON body as `T`, retrying transient failures
/// (network errors, HTTP 5xx) up to 3 times with 1s/2s/4s backoff.
/// Non-5xx HTTP errors (404 missing release, 403 rate limit) fail fast.
async fn fetch_github_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T> {
    let client = github_api_client(Duration::from_secs(15))?;
    let max_retries: u32 = 3;
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..=max_retries {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1 << (attempt - 1))).await;
        }
        let resp = match client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(anyhow::anyhow!(
                    "GitHub API request failed for {url}: {e:#}"
                ));
                continue;
            }
        };
        let status = resp.status();
        if status.is_server_error() {
            last_err = Some(anyhow::anyhow!(
                "GitHub API returned HTTP {status} for {url}"
            ));
            continue;
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "GitHub API returned HTTP {} for {}: {}",
                status,
                url,
                body.chars().take(200).collect::<String>().trim()
            );
        }
        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => {
                last_err = Some(anyhow::anyhow!(
                    "GitHub API body read failed for {url}: {e:#}"
                ));
                continue;
            }
        };
        return serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("GitHub API returned unexpected JSON for {url}: {e}"));
    }
    Err(last_err.expect("loop ran at least once"))
}

/// Latest release for `channel` from a GitHub-Releases-shaped API at `base`.
///
/// - `stable` / `enterprise`: `GET {base}/latest` — GitHub's "latest" already
///   excludes drafts and pre-releases.
/// - `alpha`: `GET {base}?per_page=30` (newest first) and take the semver-max
///   non-draft entry. The list includes both pre-releases and stable
///   releases, so this preserves the max(alpha, stable) channel semantics —
///   alpha users are never stuck behind a newer stable.
pub async fn fetch_latest_release_from_base(channel: &str, base: &str) -> Result<Release> {
    let base = base.trim_end_matches('/');
    if channel == "alpha" {
        let releases: Vec<Release> = fetch_github_json(&format!("{base}?per_page=30")).await?;
        return releases
            .into_iter()
            .filter(|r| !r.draft)
            .filter_map(|r| {
                let v = semver::Version::parse(r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name))
                    .ok()?;
                Some((v, r))
            })
            .max_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, r)| r)
            .ok_or_else(|| anyhow::anyhow!("no releases with semver tags found at {base}"));
    }
    fetch_github_json(&format!("{base}/latest")).await
}

/// Release for an exact version (`GET {base}/tags/v{version}`), used by
/// pinned installs (`kigi update --version X`) and rollbacks.
pub async fn fetch_release_for_version_from_base(version: &str, base: &str) -> Result<Release> {
    let base = base.trim_end_matches('/');
    fetch_github_json(&format!("{base}/tags/v{version}")).await
}

/// Latest release for `channel` from the production base URL
/// ([`kigi_env::update_base_url`]).
pub(crate) async fn fetch_latest_release(channel: &str) -> Result<Release> {
    fetch_latest_release_from_base(channel, &update_base_url()).await
}

/// Fetch the latest version for the configured channel without writing the
/// version cache. Use this when the caller needs to control when the cache is
/// written (e.g. auto-update should only cache after a successful install or
/// when no update is needed).
pub async fn fetch_latest_version(config: &UpdateConfig) -> Result<String> {
    fetch_latest_release(&config.channel).await?.version()
}

/// Fetch the latest version for the configured channel and cache it.
pub async fn get_latest_version(config: &UpdateConfig) -> Result<String> {
    let version = fetch_latest_version(config).await?;
    let stable_ptr = try_fetch_stable_version().await;
    write_version_cache(&version, stable_ptr.as_deref()).await;
    Ok(version)
}

/// Fetch the latest stable version for caching alongside the version, so
/// `channel_label()` can derive `[alpha]` vs `[stable]` without network I/O.
///
/// Best-effort and capped at 500 ms: the label is cosmetic, never required
/// for correctness. On slow or unreachable networks the timeout fires and we
/// return `None`; the label populates on the next successful TTL check
/// (~30 min). This keeps startup and post-install paths fast.
pub(crate) async fn try_fetch_stable_version() -> Option<String> {
    tokio::time::timeout(Duration::from_millis(500), async {
        fetch_latest_release("stable").await.ok()?.version().ok()
    })
    .await
    .unwrap_or(None)
}

// Version cache (~/.kigi/version.json)

#[derive(Debug, serde::Serialize, Deserialize)]
struct VersionCache {
    version: String,
    #[serde(default)]
    stable_version: Option<String>,
    checked_at: String,
}

impl VersionCache {
    fn is_fresh(&self, now: time::OffsetDateTime, ttl: Duration) -> bool {
        if let Ok(dt) = time::OffsetDateTime::parse(
            &self.checked_at,
            &time::format_description::well_known::Rfc3339,
        ) {
            // Clock-skew guard: future timestamps are never fresh.
            if dt > now {
                return false;
            }
            now - dt < ttl
        } else {
            false
        }
    }

    fn new(version: String, stable_version: Option<String>, now: time::OffsetDateTime) -> Self {
        let checked_at = now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| now.to_string());
        Self {
            version,
            stable_version,
            checked_at,
        }
    }
}

/// Write the version cache to disk, recording that `version` was seen at the
/// current time. Call after confirming the version is current (no update
/// needed) or after a successful install.
///
/// `stable_version` records the current stable release so that
/// `channel_label()` can derive `[alpha]` vs `[stable]` without network I/O.
pub async fn write_version_cache(version: &str, stable_version: Option<&str>) {
    let version_path = kigi_home().join("version.json");
    let now = time::OffsetDateTime::now_utc();
    let json = VersionCache::new(
        version.to_string(),
        stable_version.map(|s| s.to_string()),
        now,
    );
    if let Some(dir) = version_path.parent()
        && let Err(e) = fs::create_dir_all(dir).await
    {
        tracing::warn!("failed to create version cache directory: {}", e);
        return;
    }
    let tmp = version_path.with_extension("json.tmp");
    let data = match serde_json::to_vec_pretty(&json) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("failed to serialize version cache: {}", e);
            return;
        }
    };
    if let Err(e) = fs::write(&tmp, data).await {
        tracing::warn!("failed to write version cache tmp file: {}", e);
        return;
    }
    if let Err(e) = fs::rename(&tmp, &version_path).await {
        tracing::warn!("failed to rename version cache file: {}", e);
    }
}

/// True if `version.json` exists and is within TTL.
pub async fn is_version_cache_fresh() -> bool {
    let version_path = kigi_home().join("version.json");
    let now = time::OffsetDateTime::now_utc();
    if let Ok(version_str) = fs::read_to_string(&version_path).await
        && let Ok(version) = serde_json::from_str::<VersionCache>(&version_str)
        && version.is_fresh(now, TTL_SECONDS_BEFORE_AUTO_UPDATE)
    {
        return true;
    }
    false
}

pub use kigi_version::installed as get_installed_kigi_version;

/// Version of the managed kigi binary currently on disk, read from the
/// `~/.kigi/bin/kigi` symlink target (`../downloads/kigi-<version>-<platform>`)
/// without exec'ing anything.
///
/// Concurrent updaters (TUI background download, leader hourly checker,
/// explicit `kigi update`) decide staleness from this instead of their own
/// compiled-in version, so a binary another process already installed is
/// never downloaded a second time.
///
/// Returns `None` when there is no parseable managed symlink (Windows
/// copy-based installs, dev builds) or when the symlink is DANGLING — a
/// link whose target binary was deleted (e.g. manual `~/.kigi/downloads`
/// cleanup) must not report an installed version, or every updater would
/// claim "already up to date" forever while no runnable binary exists.
pub fn installed_on_disk_version() -> Option<String> {
    #[cfg(unix)]
    {
        let app = kigi_shell::util::kigi_home::kigi_application();
        let target = std::fs::read_link(&app).ok()?;
        // metadata() follows the symlink: Err means the target is gone
        // (dangling link) and the version it names is not actually on disk.
        std::fs::metadata(&app).ok()?;
        version_from_versioned_binary_name(target.file_name()?.to_str()?, "kigi")
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Extract the `<version>` portion of a versioned binary file name.
///
/// Handles the managed layout (`kigi-0.1.0-macos-aarch64`, including
/// pre-releases: `kigi-0.1.0-alpha.1-linux-x86_64` → `0.1.0-alpha.1`) and a
/// bare versioned name without a platform suffix (`kigi-0.1.0`): everything
/// between the `{bin_prefix}-` prefix and the first platform-OS component is
/// the version, validated as semver so unknown layouts (`kigi-latest`)
/// return `None` instead of garbage.
///
/// Shared by the disk-version probe above and `cleanup_old_downloads` in
/// `auto_update` — keep it the single place that understands this naming.
pub(crate) fn version_from_versioned_binary_name(name: &str, bin_prefix: &str) -> Option<String> {
    const PLATFORM_OS: &[&str] = &["macos", "linux", "darwin", "windows"];
    // Release archives (`kigi-<v>-<triple>.tar.gz|.zip`) are downloads, not
    // binaries — and their triple would otherwise parse as a semver
    // pre-release (`0.1.0-aarch64-apple-darwin.tar.gz` is valid semver).
    if name.ends_with(".tar.gz") || name.ends_with(".zip") {
        return None;
    }
    let suffix = name.strip_prefix(bin_prefix)?.strip_prefix('-')?;
    let parts: Vec<&str> = suffix.split('-').collect();
    let platform_start = parts
        .iter()
        .position(|p| PLATFORM_OS.contains(p))
        .unwrap_or(parts.len());
    let ver_str = parts[..platform_start].join("-");
    semver::Version::parse(&ver_str).ok()?;
    Some(ver_str)
}

/// Read the cached stable version from `~/.kigi/version.json` (sync, for display).
///
/// Returns `None` if the file doesn't exist, can't be parsed, or has no
/// `stable_version` field (e.g. written by an older binary).
pub fn cached_stable_version() -> Option<String> {
    let version_path = kigi_home().join("version.json");
    let content = std::fs::read_to_string(&version_path).ok()?;
    let gv: VersionCache = serde_json::from_str(&content).ok()?;
    gv.stable_version
}

/// Pure comparison: derive the channel name from current vs stable pointer.
///
/// Returns `Some("alpha")` when `current > stable`, `Some("stable")` when
/// `current <= stable`, or `None` when either version fails to parse.
fn derive_channel<'a>(current: &str, stable: &str) -> Option<&'a str> {
    let current_v = semver::Version::parse(current).ok()?;
    let stable_v = semver::Version::parse(stable).ok()?;
    if current_v > stable_v {
        Some("alpha")
    } else {
        Some("stable")
    }
}

/// Machine-readable channel name derived from the cached stable pointer.
///
/// Returns `Some("alpha")` when the current version is ahead of the cached
/// stable pointer, `Some("stable")` when at or behind, or `None` when no
/// cached pointer is available (first launch, old cache format, parse error).
///
/// The result is computed once and cached for the process lifetime.
pub fn channel_name() -> Option<&'static str> {
    use std::sync::OnceLock;
    static NAME: OnceLock<Option<&'static str>> = OnceLock::new();
    *NAME.get_or_init(|| {
        let stable = cached_stable_version()?;
        derive_channel(kigi_version::VERSION, &stable)
    })
}

/// Channel label derived from the cached stable pointer.
///
/// Compares the compiled-in `VERSION` against the stable pointer stored in
/// `~/.kigi/version.json` (written by the auto-updater):
/// - `" [alpha]"` when the current version is ahead of stable,
/// - `" [stable]"` when at or behind stable,
/// - `""` when no cached pointer is available (first launch, old cache format).
///
/// The result is computed once and cached for the process lifetime.
pub fn channel_label() -> &'static str {
    use std::sync::OnceLock;
    static LABEL: OnceLock<&'static str> = OnceLock::new();
    LABEL.get_or_init(|| {
        let stable = match cached_stable_version() {
            Some(s) => s,
            None => return "",
        };
        match derive_channel(kigi_version::VERSION, &stable) {
            Some("alpha") => " [alpha]",
            Some(_) => " [stable]",
            None => "",
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that a future `checked_at` timestamp (e.g. from clock skew or
    /// NTP time-warp) is never considered fresh. Without the clock-skew guard
    /// this would return true indefinitely, silently disabling auto-update.
    #[test]
    fn test_is_fresh_rejects_future_timestamp() {
        let now = time::OffsetDateTime::now_utc();
        let future = now + Duration::from_secs(600);
        let v = VersionCache::new("0.1.200".to_string(), None, future);
        assert!(
            !v.is_fresh(now, Duration::from_secs(30)),
            "Future timestamp must not be considered fresh (clock-skew guard)."
        );
    }

    /// Disk-version probe: parsing the version out of the managed install's
    /// symlink-target file name (`kigi-<version>-<platform>`).
    #[test]
    fn test_version_from_versioned_binary_name() {
        let cases: &[(&str, Option<&str>)] = &[
            ("kigi-0.2.46-darwin-arm64", Some("0.2.46")),
            ("kigi-0.1.220-linux-x86_64", Some("0.1.220")),
            ("kigi-0.2.5-windows-x86_64.exe", Some("0.2.5")),
            // Pre-releases must round-trip whole — truncating to "0.1.220"
            // would make an alpha install masquerade as the release and
            // mask alpha → stable updates.
            ("kigi-0.1.220-alpha.4-linux-x86_64", Some("0.1.220-alpha.4")),
            // no platform suffix
            ("kigi-0.1.220-alpha.4", Some("0.1.220-alpha.4")),
            // unparseable version
            ("kigi-garbage-darwin-arm64", None),
            // no platform suffix
            ("kigi-0.2.46", Some("0.2.46")),
            // wrong prefix
            ("other-0.2.46-darwin-arm64", None),
            // symlink alias, not a version
            ("kigi-latest", None),
            ("kigi", None),
            ("", None),
            // Release archives must never parse as versioned binaries, or
            // cleanup would treat them as installable versions.
            ("kigi-0.1.0-aarch64-apple-darwin.tar.gz", None),
            ("kigi-0.1.0-x86_64-pc-windows-msvc.zip", None),
        ];
        for (name, expected) in cases {
            assert_eq!(
                version_from_versioned_binary_name(name, "kigi").as_deref(),
                *expected,
                "version_from_versioned_binary_name({name:?})"
            );
        }

        // bin_prefix discrimination: a differently-prefixed binary parses
        // under its own prefix but not under "kigi".
        assert_eq!(
            version_from_versioned_binary_name("kigi-pager-0.1.5-darwin-arm64", "kigi-pager")
                .as_deref(),
            Some("0.1.5")
        );
        assert_eq!(
            version_from_versioned_binary_name("kigi-pager-0.1.5-darwin-arm64", "kigi"),
            None,
            "\"pager\" is not a version"
        );
    }

    // GitHub release JSON — wire-shape invariants
    //
    // Fixtures mirror the real GitHub REST API response for
    // GET /repos/{owner}/{repo}/releases/latest:
    // https://docs.github.com/en/rest/releases/releases#get-the-latest-release

    #[test]
    fn test_release_json_parses_github_wire_shape() {
        let json = r#"{
            "url": "https://api.github.com/repos/ZacharyZhang-NY/Kigi-CLI/releases/1",
            "tag_name": "v0.1.0",
            "name": "Kigi 0.1.0",
            "draft": false,
            "prerelease": false,
            "assets": [
                {
                    "name": "kigi-0.1.0-aarch64-apple-darwin.tar.gz",
                    "browser_download_url": "https://github.com/ZacharyZhang-NY/Kigi-CLI/releases/download/v0.1.0/kigi-0.1.0-aarch64-apple-darwin.tar.gz",
                    "size": 123,
                    "content_type": "application/gzip"
                },
                {
                    "name": "SHA256SUMS",
                    "browser_download_url": "https://github.com/ZacharyZhang-NY/Kigi-CLI/releases/download/v0.1.0/SHA256SUMS"
                }
            ]
        }"#;
        let r: Release = serde_json::from_str(json).unwrap();
        assert_eq!(r.tag_name, "v0.1.0");
        assert_eq!(r.version().unwrap(), "0.1.0");
        assert!(!r.draft);
        assert!(!r.prerelease);
        assert_eq!(r.assets.len(), 2);
        let asset = r.asset("kigi-0.1.0-aarch64-apple-darwin.tar.gz").unwrap();
        assert_eq!(
            asset.browser_download_url,
            "https://github.com/ZacharyZhang-NY/Kigi-CLI/releases/download/v0.1.0/kigi-0.1.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            r.asset("SHA256SUMS").unwrap().name,
            "SHA256SUMS",
            "checksum asset resolved by exact name"
        );
    }

    #[test]
    fn test_release_version_accepts_bare_and_v_prefixed_tags() {
        let mk = |tag: &str| Release {
            tag_name: tag.to_string(),
            draft: false,
            prerelease: false,
            assets: vec![],
        };
        assert_eq!(mk("v0.1.0").version().unwrap(), "0.1.0");
        assert_eq!(mk("0.1.0").version().unwrap(), "0.1.0");
        assert_eq!(mk("v0.2.0-alpha.3").version().unwrap(), "0.2.0-alpha.3");
        assert!(mk("release-1").version().is_err());
        assert!(mk("").version().is_err());
    }

    #[test]
    fn test_release_missing_asset_error_lists_available() {
        let r = Release {
            tag_name: "v0.1.0".to_string(),
            draft: false,
            prerelease: false,
            assets: vec![ReleaseAsset {
                name: "SHA256SUMS".to_string(),
                browser_download_url: "https://example.test/SHA256SUMS".to_string(),
            }],
        };
        let err = r
            .asset("kigi-0.1.0-x86_64-unknown-linux-gnu.tar.gz")
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no asset named"), "msg: {msg}");
        assert!(msg.contains("SHA256SUMS"), "must list available: {msg}");
    }

    #[test]
    fn test_release_defaults_for_absent_optional_fields() {
        // Minimal object: only tag_name. draft/prerelease default false,
        // assets default empty (serde(default)).
        let r: Release = serde_json::from_str(r#"{"tag_name":"v0.1.0"}"#).unwrap();
        assert!(!r.draft && !r.prerelease && r.assets.is_empty());
        // tag_name is required.
        assert!(serde_json::from_str::<Release>(r#"{"assets":[]}"#).is_err());
    }

    // derive_channel — invariant matrix
    //
    // Tests the pure comparison logic that determines [alpha] vs [stable].
    // Covers current 0.1.X-alpha.N, future 0.2.X, edge cases, and errors.

    #[test]
    fn test_derive_channel_matrix() {
        // (current, stable_pointer, expected_channel)
        let cases: &[(&str, &str, Option<&str>)] = &[
            // Current 0.1.X workflow
            // alpha ahead of stable
            ("0.1.220-alpha.2", "0.1.219", Some("alpha")),
            // stable user on latest
            ("0.1.219", "0.1.219", Some("stable")),
            // stable user behind latest
            ("0.1.218", "0.1.219", Some("stable")),
            // pointer matches exactly
            ("0.1.220-alpha.2", "0.1.220-alpha.2", Some("stable")),
            // semver: release > pre-release
            ("0.1.220-alpha.2", "0.1.220", Some("stable")),
            // Future 0.2.X workflow
            // alpha ahead of stable
            ("0.2.5", "0.2.3", Some("alpha")),
            // promoted to stable
            ("0.2.5", "0.2.5", Some("stable")),
            // behind stable
            ("0.2.3", "0.2.5", Some("stable")),
            // first release, both 0.2.0
            ("0.2.0", "0.2.0", Some("stable")),
            // Cross-regime upgrade
            // new regime ahead of old stable
            ("0.2.0", "0.1.219", Some("alpha")),
            // old pre-release < new stable
            ("0.1.220-alpha.2", "0.2.0", Some("stable")),
            // Error cases
            // unparseable current
            ("garbage", "0.1.219", None),
            // unparseable stable
            ("0.1.219", "garbage", None),
            // empty current
            ("", "0.1.219", None),
            // empty stable
            ("0.1.219", "", None),
        ];

        for (current, stable, expected) in cases {
            let result = derive_channel(current, stable);
            assert_eq!(
                result, *expected,
                "derive_channel({:?}, {:?}) = {:?}, expected {:?}",
                current, stable, result, expected,
            );
        }
    }

    // VersionCache JSON shape — backward compatibility invariants

    #[test]
    fn test_version_json_backward_compat() {
        // Old format (no stable_version) must parse — serde(default) fills None.
        let old = r#"{"version":"0.1.180","checked_at":"2026-04-22T10:30:00Z"}"#;
        let v: VersionCache = serde_json::from_str(old).unwrap();
        assert_eq!(v.version, "0.1.180");
        assert!(v.stable_version.is_none());

        // New format with stable_version round-trips correctly.
        let now = time::OffsetDateTime::now_utc();
        let new = VersionCache::new("0.2.5".to_string(), Some("0.2.3".to_string()), now);
        let json = serde_json::to_string(&new).unwrap();
        let parsed: VersionCache = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version, "0.2.5");
        assert_eq!(parsed.stable_version.as_deref(), Some("0.2.3"));

        // checked_at must be valid RFC3339.
        assert!(
            time::OffsetDateTime::parse(
                &parsed.checked_at,
                &time::format_description::well_known::Rfc3339,
            )
            .is_ok()
        );

        // Unknown fields are ignored (forward-compat).
        let future = r#"{"version":"0.1.180","checked_at":"2026-04-22T10:30:00Z","future":"ok"}"#;
        assert!(serde_json::from_str::<VersionCache>(future).is_ok());

        // Missing required field (checked_at) is rejected.
        let missing = r#"{"version":"0.1.180"}"#;
        assert!(serde_json::from_str::<VersionCache>(missing).is_err());
    }

    // is_fresh — TTL boundary invariants

    #[test]
    fn test_is_fresh_ttl_boundaries() {
        let now = time::OffsetDateTime::now_utc();
        let v = VersionCache::new("0.1.200".to_string(), None, now);

        // Within TTL → fresh
        assert!(v.is_fresh(now, Duration::from_secs(60)));
        assert!(v.is_fresh(now + Duration::from_secs(29), Duration::from_secs(30)));

        // At TTL boundary → NOT fresh (strict <)
        assert!(!v.is_fresh(now + Duration::from_secs(30), Duration::from_secs(30)));

        // Past TTL → not fresh
        assert!(!v.is_fresh(now + Duration::from_secs(31), Duration::from_secs(30)));

        // Zero TTL → never fresh
        assert!(!v.is_fresh(now, Duration::ZERO));

        // Malformed timestamp → not fresh
        let bad = VersionCache {
            version: "0.1.200".to_string(),
            stable_version: None,
            checked_at: "not-rfc3339".to_string(),
        };
        assert!(!bad.is_fresh(now, Duration::from_secs(60)));
    }

    // UpdateConfig defaults

    #[test]
    fn test_update_config_default_channel_is_stable() {
        let cfg = UpdateConfig::from_environment();
        assert_eq!(cfg.channel, "stable");
    }
}
