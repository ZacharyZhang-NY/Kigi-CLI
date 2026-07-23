use anyhow::{Context, Result};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::io::AsyncWriteExt;

use crate::version::{
    Release, UpdateConfig, fetch_latest_version, get_installed_kigi_version, get_latest_version,
    is_version_cache_fresh, try_fetch_stable_version, write_version_cache,
};
use kigi_shell::util::config;
use kigi_shell::util::kigi_home::{kigi_application, kigi_home};

#[derive(Clone, Copy, Debug)]
pub enum UpdateRunMode {
    Blocking,
    NonBlocking,
}

const PROMPT_UPDATE_NOW: &str = "Update now? [Y/n/d]";
const MSG_AUTO_UPDATE_BACKGROUND: &str = "Auto-update running in background.";
const MSG_RUN_UPDATE_MANUAL: &str = "Run `kigi update` to get the latest version.";

/// Manual-install one-liner for this platform's bootstrap installer
/// (install.sh / install.ps1 hosted at the repo root, PRD F8).
fn manual_install_cmd() -> &'static str {
    if cfg!(windows) {
        "irm https://raw.githubusercontent.com/ZacharyZhang-NY/Kigi-CLI/main/install.ps1 | iex"
    } else {
        "curl -fsSL https://raw.githubusercontent.com/ZacharyZhang-NY/Kigi-CLI/main/install.sh | sh"
    }
}

/// Build a reinstall hint for a known installer type. Every installer is
/// "internal" (GitHub Releases) today; the parameter survives so the hint
/// stays correct if another backend ever returns.
fn reinstall_hint(_installer: &str) -> String {
    format!("Please reinstall via:\n  {}", manual_install_cmd())
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub installer: Option<String>,
    pub channel: String,
    pub auto_update: Option<bool>,
    pub error: Option<String>,
}

/// Format and print an [`UpdateStatus`] to stdout.
pub fn print_update_status(status: &UpdateStatus, json: bool) -> anyhow::Result<()> {
    if json {
        let payload = serde_json::to_string(status)?;
        println!("{payload}");
        return Ok(());
    }

    if let Some(error) = status.error.as_deref() {
        println!("Kigi - v{} [{}]", status.current_version, status.channel);
        println!("Update check failed: {error}");
        return Ok(());
    }

    let channel_label = format!(" [{}]", status.channel);

    if status.update_available {
        if let Some(latest_version) = status.latest_version.as_deref() {
            println!(
                "A new version of Kigi is available: {} -> {}{}",
                status.current_version, latest_version, channel_label
            );
        } else {
            println!("A new version of Kigi is available.");
        }
        return Ok(());
    }

    if let Some(latest_version) = status.latest_version.as_deref() {
        println!(
            "Kigi - v{} (latest: {}){}",
            status.current_version, latest_version, channel_label
        );
        return Ok(());
    }

    println!("Kigi - v{}{}", status.current_version, channel_label);
    Ok(())
}

pub async fn check_update_status(update_config: &UpdateConfig) -> UpdateStatus {
    let installer = get_installer().await.map(|value| value.to_string());
    let current_version = get_installed_kigi_version();
    let current_config = config::load_config().await;
    let auto_update = current_config.cli.auto_update;
    let channel = update_config.channel.clone();

    let Some(ref _inst) = installer else {
        return UpdateStatus {
            current_version,
            latest_version: None,
            update_available: false,
            installer,
            channel,
            auto_update,
            error: None,
        };
    };

    match get_latest_version(update_config).await {
        Ok(latest_version) => {
            let mut error = None;
            // --check reports upgrades only; a rolled-back pointer isn't a "new version" to advertise here (auto-update converges separately).
            let allow_downgrade = false;
            let update_available =
                match needs_update(&current_version, &latest_version, &channel, allow_downgrade) {
                    Some(value) => value,
                    None => {
                        // Distinguish parse failure from unsupported channel for clearer diagnostics.
                        let parse_ok = semver::Version::parse(&current_version).is_ok()
                            && semver::Version::parse(&latest_version).is_ok();
                        error = Some(if parse_ok {
                            format!(
                                "Unsupported release channel '{}' (current={}, latest={}). \
                             Supported channels: stable, alpha, enterprise.",
                                channel, current_version, latest_version
                            )
                        } else {
                            format!(
                                "Failed to parse versions (current={}, latest={})",
                                current_version, latest_version
                            )
                        });
                        false
                    }
                };

            UpdateStatus {
                current_version,
                latest_version: Some(latest_version),
                update_available,
                installer,
                channel,
                auto_update,
                error,
            }
        }
        Err(err) => UpdateStatus {
            current_version,
            latest_version: None,
            update_available: false,
            installer,
            channel,
            auto_update,
            error: Some(err.to_string()),
        },
    }
}

/// Installer + version the leader/background path should converge to: an
/// upgrade OR an authoritative-installer rollback. `None` means stay put.
/// Gates on the installer (via `installer_allows_downgrade`) so the decision
/// depends on the installer, never the caller.
pub async fn auto_update_target(update_config: &UpdateConfig) -> Option<(&'static str, String)> {
    let installer = get_installer().await?;
    let current = get_installed_kigi_version();
    let latest = fetch_latest_version(update_config).await.ok()?;
    needs_update(
        &current,
        &latest,
        &update_config.channel,
        installer_allows_downgrade(installer),
    )
    .unwrap_or(false)
    .then_some((installer, latest))
}

/// Outcome of [`ensure_latest_on_disk`].
#[derive(Debug)]
pub struct EnsureLatestOutcome {
    /// Version this call downloaded and installed; `None` when the disk was
    /// already current (or there was no installer).
    pub installed: Option<String>,
    /// The running process differs from what is now on disk in the channel's
    /// update direction — the caller should relaunch onto the on-disk binary.
    pub relaunch_needed: bool,
}

/// One leader auto-update pass: converge the on-disk install to the latest
/// release (downloading **only** when the disk is actually behind it), then
/// report whether the running process should relaunch onto the on-disk binary.
///
/// Unlike [`run_update`] this never uses the compiled-in version for the
/// download decision — a binary already installed by another process (TUI
/// background download, explicit `kigi update`) is reused as-is. This both
/// removes the duplicate download in leader mode and stops the pre-fix
/// hourly re-download while a busy leader keeps deferring its relaunch.
///
/// When the disk version is unknowable ([`disk_version_for_installer`]:
/// Windows copy-based installs, dev builds), this degrades to the pre-fix
/// behavior — download when the *running* process is stale, relaunch only
/// after a download this pass actually installed something.
pub async fn ensure_latest_on_disk(update_config: &UpdateConfig) -> Result<EnsureLatestOutcome> {
    let mut outcome = EnsureLatestOutcome {
        installed: None,
        relaunch_needed: false,
    };
    let Some(installer) = get_installer().await else {
        return Ok(outcome);
    };
    let allow_downgrade = installer_allows_downgrade(installer);
    let latest = fetch_latest_version(update_config).await?;

    let effective_current =
        disk_version_for_installer(installer).unwrap_or_else(get_installed_kigi_version);
    if needs_update(
        &effective_current,
        &latest,
        &update_config.channel,
        allow_downgrade,
    )
    .unwrap_or(false)
    {
        run_install_script(installer, Some(&latest), update_config).await?;
        outcome.installed = Some(latest.clone());
    }

    // Relaunch when the running binary differs from what's on disk in the
    // channel's update direction — covers binaries installed by other
    // processes, not just the install above.
    let running = get_installed_kigi_version();
    if let Some(disk_now) =
        disk_version_for_installer(installer).or_else(|| outcome.installed.clone())
    {
        outcome.relaunch_needed =
            needs_update(&running, &disk_now, &update_config.channel, allow_downgrade)
                .unwrap_or(false);
    }
    Ok(outcome)
}

/// Disk-version probe gated on the installer actually maintaining the
/// managed `~/.kigi/bin/kigi` symlink. Only the internal (GitHub Releases)
/// installer writes that symlink; unknown installers report no trustworthy
/// disk version.
fn disk_version_for_installer(installer: &str) -> Option<String> {
    match installer {
        "internal" => crate::version::installed_on_disk_version(),
        _ => None,
    }
}

fn env_installer() -> Option<&'static str> {
    if let Ok(v) = std::env::var("KIGI_INSTALLER") {
        return match v.to_ascii_lowercase().as_str() {
            "internal" => Some("internal"),
            _ => None,
        };
    }
    if std::env::var_os("KIGI_MANAGED_BY_INTERNAL").is_some() {
        return Some("internal");
    }
    None
}

/// Resolve the active installer backend. Every supported install path
/// (install.sh, install.ps1, self-update) is "internal" — binaries from this
/// repo's GitHub Releases (PRD F8: no PyPI/npm packages).
pub async fn get_installer() -> Option<&'static str> {
    if let Some(i) = env_installer() {
        return Some(i);
    }
    // Any persisted installer value maps to the single supported backend.
    let _ = config::load_config().await.cli.installer;
    Some("internal")
}

fn needs_update(current: &str, target: &str, channel: &str, allow_downgrade: bool) -> Option<bool> {
    let current = semver::Version::parse(current).ok()?;
    let target = semver::Version::parse(target).ok()?;
    match channel {
        "stable" | "enterprise" => {
            if !target.pre.is_empty() {
                tracing::warn!(
                    %current, %target,
                    channel = %channel,
                    "stable/enterprise channel received pre-release candidate, rejecting"
                );
                return Some(false);
            }
            if !current.pre.is_empty() {
                return Some(true);
            }
        }
        "alpha" => {}
        _ => return None,
    }
    Some(if allow_downgrade {
        target != current
    } else {
        target > current
    })
}

/// Returns `true` for installer backends whose version source is
/// authoritative (this repo's GitHub Releases), meaning a release rollback
/// (deleted/yanked latest) is intentional and should trigger a client
/// downgrade. Unknown backends never downgrade.
fn installer_allows_downgrade(installer: &str) -> bool {
    installer == "internal"
}

/// Result of a background update availability check.
#[derive(Debug, Clone)]
pub struct UpdateAvailable {
    /// The latest version string (e.g. "0.1.2").
    pub latest_version: String,
}

/// Outcome of [`check_update_background`].
pub struct BackgroundUpdateCheck {
    /// `Some` when the *running* binary is older than the latest release —
    /// drives the in-TUI restart hint regardless of who downloads the binary.
    pub update: Option<UpdateAvailable>,
    /// Handle to the background `kigi update` child, `Some` only when a
    /// download was actually started (the on-disk install was behind the
    /// latest release). The TUI parks this and `wait()`s on it at
    /// quit-for-update time instead of spawning a second downloader.
    pub download: Option<tokio::process::Child>,
}

impl BackgroundUpdateCheck {
    fn none() -> Self {
        Self {
            update: None,
            download: None,
        }
    }
}

/// Check for available updates without blocking the TUI startup.
///
/// Sets [`BackgroundUpdateCheck::update`] when the running binary is older
/// than the latest release. If `auto_update` is enabled **and the on-disk
/// install is also behind it**, kicks off a non-blocking download (spawns
/// `kigi update` as a detached child process) so the new binary is ready
/// when the user quits and relaunches. When another process (an earlier TUI,
/// the leader's hourly checker) already put the target version on disk, no
/// download is started — only the restart hint is surfaced.
pub async fn check_update_background(update_config: &UpdateConfig) -> BackgroundUpdateCheck {
    let Some(installer) = get_installer().await else {
        return BackgroundUpdateCheck::none();
    };

    if is_version_cache_fresh().await {
        return BackgroundUpdateCheck::none();
    }

    let current_config = config::load_config().await;
    if current_config.cli.auto_update == Some(false) {
        return BackgroundUpdateCheck::none();
    }

    let current_version = get_installed_kigi_version();
    let latest_version = match fetch_latest_version(update_config).await {
        Ok(v) => v,
        Err(_) => return BackgroundUpdateCheck::none(),
    };

    let allow_downgrade = installer_allows_downgrade(installer);
    if !needs_update(
        &current_version,
        &latest_version,
        &update_config.channel,
        allow_downgrade,
    )
    .unwrap_or(false)
    {
        let stable_ptr = try_fetch_stable_version().await;
        write_version_cache(&latest_version, stable_ptr.as_deref()).await;
        return BackgroundUpdateCheck::none();
    }

    // Only download when the on-disk install is behind the latest release;
    // the running process being stale (checked above) just means "show the
    // restart hint". The quit-for-update path's `kigi update` child resolves
    // to "Already up to date" against the same disk state.
    let disk_needs_download = match disk_version_for_installer(installer) {
        Some(disk) => needs_update(
            &disk,
            &latest_version,
            &update_config.channel,
            allow_downgrade,
        )
        .unwrap_or(true),
        None => true,
    };

    // Kick off a non-blocking download so the binary is ready when the
    // user restarts (or accepts the in-TUI restart prompt).
    let download = if disk_needs_download {
        match run_update_subcommand(UpdateRunMode::NonBlocking).await {
            Ok(child) => child,
            Err(e) => {
                tracing::warn!("Background update download failed to start: {e}");
                None
            }
        }
    } else {
        tracing::info!(
            latest_version = %latest_version,
            "Background update: target already on disk, skipping download"
        );
        None
    };

    BackgroundUpdateCheck {
        update: Some(UpdateAvailable { latest_version }),
        download,
    }
}

/// Returns Ok(true) if a blocking update ran; otherwise Ok(false).
pub async fn run_update_if_available(
    run_mode: UpdateRunMode,
    interactive: bool,
    update_config: &UpdateConfig,
) -> Result<bool> {
    let installer = get_installer().await;
    if installer.is_none() {
        // Skip update check if no known installer.
        return Ok(false);
    }

    if is_version_cache_fresh().await {
        return Ok(false);
    }

    let current_config = config::load_config().await;

    // Skip update check if auto-update is explicitly disabled.
    if current_config.cli.auto_update == Some(false) {
        return Ok(false);
    }

    // Resolve effective auto_update: None defaults to true (first-run).
    let auto_update = current_config.cli.auto_update.unwrap_or(true);

    if current_config.cli.auto_update.is_none()
        && let Err(e) = config::update_config(|st| {
            if st.cli.auto_update.is_none() {
                st.cli.auto_update = Some(true);
            }
        })
        .await
    {
        tracing::warn!("Failed to save auto-update setting: {}", e);
    }

    let current_version = get_installed_kigi_version();
    // installer is guaranteed Some by the guard at the top of this function.
    let inst = installer.unwrap();
    // Fetch without writing version.json — we only cache after confirming the
    // update is not needed or after a successful blocking install. This prevents
    // a failed background download from suppressing retries for the TTL window.
    let latest_version = match fetch_latest_version(update_config).await {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };
    if !needs_update(
        &current_version,
        &latest_version,
        &update_config.channel,
        installer_allows_downgrade(inst),
    )
    .unwrap_or(false)
    {
        let stable_ptr = try_fetch_stable_version().await;
        write_version_cache(&latest_version, stable_ptr.as_deref()).await;
        return Ok(false);
    }

    let channel_label = format!(" [{}]", update_config.channel);
    if auto_update {
        eprintln!(
            "A new version of Kigi is available: {} -> {}{}",
            current_version, latest_version, channel_label
        );
        if interactive {
            if let Err(e) = run_update_subcommand(run_mode).await {
                eprintln!("Update failed: {}", e);
            } else if matches!(run_mode, UpdateRunMode::Blocking) {
                return Ok(true);
            } else {
                eprintln!("{}", MSG_AUTO_UPDATE_BACKGROUND);
                return Ok(false);
            }
        } else if let Err(e) = run_update_subcommand(run_mode).await {
            eprintln!("Update failed: {}", e);
        } else if matches!(run_mode, UpdateRunMode::Blocking) {
            return Ok(true);
        }
        return Ok(false);
    } else {
        if current_config
            .cli
            .dismissed_version
            .as_deref()
            .is_some_and(|v| v == latest_version)
        {
            return Ok(false);
        }
        eprintln!(
            "A new version of Kigi is available: {} -> {}{}",
            current_version, latest_version, channel_label
        );
        if interactive {
            eprintln!("{}", PROMPT_UPDATE_NOW);
            let mut line = String::new();
            if io::stdin().read_line(&mut line).is_ok() {
                let ans = line.trim().to_ascii_lowercase();
                if ans.is_empty() || ans == "y" || ans == "yes" {
                    if let Err(e) = run_update_subcommand(run_mode).await {
                        eprintln!("Update failed: {}", e);
                    } else if matches!(run_mode, UpdateRunMode::Blocking) {
                        return Ok(true);
                    } else {
                        eprintln!("{}", MSG_AUTO_UPDATE_BACKGROUND);
                        return Ok(false);
                    }
                } else if ans == "d" || ans == "dismiss" {
                    let dismissed = latest_version.clone();
                    if let Err(e) = config::update_config(|st| {
                        st.cli.dismissed_version = Some(dismissed);
                    })
                    .await
                    {
                        tracing::warn!("Failed to save dismissed version: {}", e);
                    }
                }
            }
        } else {
            eprintln!("{}", MSG_RUN_UPDATE_MANUAL);
        }
    }
    Ok(false)
}

/// Launch "kigi update" in blocking or non-blocking mode.
///
/// In `NonBlocking` mode the spawned child's handle is returned so the caller
/// can later `wait()` on the in-flight download (e.g. the TUI's
/// quit-for-update path) instead of blind-spawning a second downloader.
/// Dropping the handle does not kill the child (`kill_on_drop` is off), so
/// callers that don't care can ignore it. `Blocking` mode returns `None`.
async fn run_update_subcommand(run_mode: UpdateRunMode) -> Result<Option<tokio::process::Child>> {
    let exe = std::env::current_exe()?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("update");
    match run_mode {
        UpdateRunMode::Blocking => {
            // stderr must be null, not piped: `.status()` does not drain
            // pipes, so if the child writes more than the OS pipe buffer
            // (~16 KB macOS / ~64 KB Linux) to stderr (e.g. download
            // progress bars), the child blocks on the write while the
            // parent blocks on waitpid — deadlocking both processes.
            // With `panic = "abort"`, the blocked child eventually
            // receives SIGABRT.
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                // inherit, not piped: the TUI is already restored so the
                // parent's stderr fd is a normal terminal. inherit lets
                // the child's diagnostic output reach the user. piped +
                // status() would immediately close the read end → EPIPE
                // → panic → SIGABRT (signal 6) under panic=abort.
                .stderr(Stdio::inherit());
            // No detach: the child must stay in the foreground process group so Ctrl+C cancels it with the parent; the atomic install protocol makes mid-download kills safe.
            let status = cmd.status().await?;
            if !status.success() {
                anyhow::bail!("kigi update failed with {}", status);
            }
            Ok(None)
        }
        UpdateRunMode::NonBlocking => {
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // Detach = new session (Ctrl+C isolation), not handle abandonment:
            // the child is still ours to wait() on.
            kigi_tools::util::detach_command(&mut cmd);
            let child = cmd.spawn()?;
            Ok(Some(child))
        }
    }
}

/// Resolve the kigi binary path for re-execution after an update.
///
/// `current_exe()` resolves symlinks via `/proc/self/exe` (see proc(5)),
/// so it returns the old versioned target after a symlink swap.
/// Prefer `~/.kigi/bin/kigi` which always points to the latest version.
fn resolve_restart_exe() -> Result<PathBuf> {
    let canonical = kigi_application();
    if canonical.exists() {
        return Ok(canonical);
    }
    Ok(std::env::current_exe()?)
}

/// Restart kigi with the original command-line arguments to pick up the update.
pub fn restart_kigi() -> Result<()> {
    let exe = resolve_restart_exe()?;
    let mut cmd = std::process::Command::new(exe);
    for arg in std::env::args_os().skip(1) {
        cmd.arg(arg);
    }
    cmd.env_clear();
    cmd.envs(std::env::vars_os().filter(|(k, _)| k != "KIGI_AUTO_UPDATE"));
    eprintln!("Restarting Kigi...");

    // Use exec on Unix to replace the current process, avoiding stdio issues
    // when the parent exits. On Windows, fall back to spawn + exit.
    #[cfg(unix)]
    {
        // Flush output before exec to ensure messages are visible
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
        let err = cmd.exec();
        // exec only returns if there was an error
        anyhow::bail!("Failed to exec: {}", err);
    }

    #[cfg(not(unix))]
    {
        // Flush output before exit to ensure messages are visible
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let _ = cmd.spawn()?;
        std::process::exit(0);
    }
}

pub async fn run_install_script(
    installer: &str,
    target: Option<&str>,
    update_config: &UpdateConfig,
) -> Result<()> {
    let result = install_internal(target, update_config).await;
    if result.is_ok() {
        remove_stale_models_cache().await;
    }
    result.map_err(|e| {
        anyhow::anyhow!(
            "Auto-update failed: {:#}\n\n{}",
            e,
            reinstall_hint(installer)
        )
    })
}

/// Detect the current platform (os, arch) for versioned on-disk binary names
/// (`kigi-<version>-<os>-<arch>`).
pub(crate) fn detect_platform() -> Result<(&'static str, &'static str)> {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        anyhow::bail!("Unsupported OS");
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        anyhow::bail!("Unsupported architecture");
    };
    Ok((os, arch))
}

/// Rust target triple for this build — the key that maps a platform to its
/// release-asset name. Must stay in lockstep with the five targets built by
/// `.github/workflows/release.yml` and the tables in install.sh/install.ps1.
pub(crate) fn target_triple() -> Result<&'static str> {
    let triple = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else {
        anyhow::bail!("no released kigi artifact for this platform")
    };
    Ok(triple)
}

/// Release archives are tar.gz everywhere except Windows (zip).
pub(crate) const ARCHIVE_EXT: &str = if cfg!(windows) { "zip" } else { "tar.gz" };

/// Release-asset archive name for `version` on this platform:
/// `kigi-<version>-<target-triple>.{tar.gz|zip}` (PRD F8).
pub(crate) fn release_asset_name(version: &str) -> Result<String> {
    Ok(format!("kigi-{version}-{}.{ARCHIVE_EXT}", target_triple()?))
}

/// Name of the checksum manifest asset attached to every release.
pub(crate) const SHA256SUMS_ASSET: &str = "SHA256SUMS";

/// Age past which a leftover `.tmp` download file (or a freshly-renamed
/// versioned binary) is considered abandoned (crashed/killed updater) and
/// safe for `cleanup_old_downloads` to sweep. Generous compared to the
/// longest plausible download (per-request budget is
/// [`DOWNLOAD_REQUEST_TIMEOUT`]; the leader check+download pass matches) so
/// a concurrent updater's in-flight or just-landed file is never deleted
/// out from under it.
const STALE_TMP_AGE: Duration = Duration::from_secs(60 * 60);

/// Total timeout for a CLI artifact download request (including body).
const DOWNLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Unique temp path for an in-flight download of `dest`.
///
/// Appends `.{pid}-{seq}.tmp` to the FULL file name instead of using
/// `Path::with_extension`, which treats everything after the last dot of the
/// versioned name as the extension (`kigi-0.1.1-linux-x86_64` →
/// `kigi-0.1.tmp`) and therefore collides for every `0.1.x` version. The PID
/// plus a per-process counter makes the name unique per download attempt —
/// across processes (two updaters racing in the same instant, the accepted
/// lock-free residual race) and within one process — so no racer can ever
/// rename another's half-written temp file into place. Leftovers older than
/// [`STALE_TMP_AGE`] are swept by `cleanup_old_downloads`.
fn tmp_download_path(dest: &Path) -> PathBuf {
    unique_temp_sibling(dest, "tmp")
}

/// Unique temp path `<base>.{pid}-{seq}.{ext}`, appended to the full name so a
/// versioned base like `kigi-0.1.1` doesn't collide via `with_extension`.
/// PID + per-process counter keep racing updaters from clobbering each other.
fn unique_temp_sibling(base: &Path, ext: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut name = base
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(
        ".{}-{}.{ext}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    base.with_file_name(name)
}

/// Set `+x` on the temp file before renaming onto `dest`, so a concurrent
/// same-version installer never execs `dest` while it is still 0644.
async fn publish_downloaded_artifact(tmp: &Path, dest: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o755)).await?;
    }
    tokio::fs::rename(tmp, dest).await?;
    Ok(())
}

/// Files smaller than this are not worth fragmenting across parallel chunks.
const PARALLEL_DOWNLOAD_MIN_BYTES: u64 = 16 * 1024 * 1024;

/// Pick chunk count from file size: 1 chunk per 16 MiB, capped at 8.
fn parallel_chunk_count(size: u64) -> u64 {
    let size_mb = size / (1024 * 1024);
    (size_mb / 16).clamp(1, 8)
}

/// HTTP client for release-asset downloads. GitHub requires a `User-Agent`
/// on every request; asset downloads follow redirects to the CDN.
fn asset_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("kigi/", env!("CARGO_PKG_VERSION")))
        .timeout(DOWNLOAD_REQUEST_TIMEOUT)
        .build()?)
}

/// Try a parallel byte-range download to `dest`. Returns Err if the server
/// doesn't advertise a Content-Length, the file is too small to be worth
/// splitting, the range request is rejected, or any chunk transfer fails.
/// The caller is expected to fall back to a single-connection download on Err.
async fn try_parallel_download(url: &str, dest: &Path, with_progress: bool) -> Result<()> {
    let client = asset_client()?;

    let head = client.head(url).send().await?;
    if !head.status().is_success() {
        anyhow::bail!("HEAD failed: HTTP {}", head.status());
    }
    let size = head
        .content_length()
        .ok_or_else(|| anyhow::anyhow!("response missing Content-Length"))?;
    if size < PARALLEL_DOWNLOAD_MIN_BYTES {
        anyhow::bail!("file too small for parallel download ({} bytes)", size);
    }

    let n_chunks = parallel_chunk_count(size);
    if n_chunks < 2 {
        anyhow::bail!(
            "file size yields {} chunk(s); not worth parallelizing",
            n_chunks
        );
    }
    let chunk_size = size.div_ceil(n_chunks);

    let pb = if with_progress {
        let pb = ProgressBar::new(size);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("  {bar:30.cyan/dim} {bytes}/{total_bytes} ({eta})")
                .unwrap()
                .progress_chars("━╸─"),
        );
        Some(pb)
    } else {
        None
    };

    let tmp = tmp_download_path(dest);
    // Pre-allocate so each task can seek+write to its own range concurrently.
    // One blocking-pool hop instead of two per tokio::fs call.
    let tmp_for_alloc = tmp.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let f = std::fs::File::create(&tmp_for_alloc)?;
        f.set_len(size)?;
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("blocking pre-allocate task panicked: {e}"))??;

    let tasks = (0..n_chunks).map(|i| {
        let start = i * chunk_size;
        let end = std::cmp::min(start + chunk_size, size) - 1;
        let url = url.to_string();
        let tmp = tmp.clone();
        let client = client.clone();
        let pb = pb.clone();
        async move { download_range(&client, &url, &tmp, start, end, pb.as_ref()).await }
    });
    let result = futures::future::try_join_all(tasks).await;

    if let Some(pb) = &pb {
        pb.finish_and_clear();
    }

    match result {
        Ok(_) => {
            publish_downloaded_artifact(&tmp, dest).await?;
            Ok(())
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

/// Fetch bytes `[start, end]` (inclusive) of `url` and write them at `start`
/// in `dest`. Errors if the server doesn't return `206 Partial Content`.
///
/// Streams from the network into a `Vec<u8>` (so progress ticks smoothly as
/// bytes arrive), then issues a single `spawn_blocking` per chunk to do the
/// open + seek + write_all in `std::fs`. This avoids the per-write hop into
/// tokio's blocking pool that `tokio::fs::File::write_all` performs on every
/// ~8 KiB Bytes item from `bytes_stream()`.
async fn download_range(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    start: u64,
    end: u64,
    progress: Option<&ProgressBar>,
) -> Result<()> {
    let resp = client
        .get(url)
        .header("Range", format!("bytes={}-{}", start, end))
        .send()
        .await?;
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        anyhow::bail!("range request rejected: HTTP {}", resp.status());
    }
    let mut buf = Vec::with_capacity((end - start + 1) as usize);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if let Some(pb) = progress {
            pb.inc(chunk.len() as u64);
        }
        buf.extend_from_slice(&chunk);
    }
    let dest = dest.to_owned();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(&dest)?;
        f.seek(SeekFrom::Start(start))?;
        f.write_all(&buf)?;
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("blocking write task panicked: {e}"))??;
    Ok(())
}

/// Download a file from `url` to `dest` with a terminal progress bar.
///
/// If the server provides a `Content-Length` header, a determinate bar is shown
/// with bytes downloaded, total size, and ETA. Otherwise a spinner with a byte
/// counter is used as a fallback.
#[doc(hidden)]
pub async fn download_with_progress(url: &str, dest: &Path) -> Result<()> {
    // Try parallel byte-range first. Falls through to single-connection on any
    // failure (HEAD missing Content-Length, ranges rejected, partial-fetch error).
    match try_parallel_download(url, dest, true).await {
        Ok(()) => return Ok(()),
        Err(e) => {
            tracing::debug!("parallel download failed, falling back to single connection: {e}")
        }
    }

    let client = asset_client()?;
    let resp = client.get(url).send().await?;

    if !resp.status().is_success() {
        anyhow::bail!("Download failed: HTTP {}", resp.status());
    }

    let total_size = resp.content_length();

    let pb = if let Some(size) = total_size {
        let pb = ProgressBar::new(size);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("  {bar:30.cyan/dim} {bytes}/{total_bytes} ({eta})")
                .unwrap()
                .progress_chars("━╸─"),
        );
        pb
    } else {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("  {spinner:.cyan} {bytes} downloaded")
                .unwrap(),
        );
        pb.enable_steady_tick(Duration::from_millis(100));
        pb
    };

    // Stream to a temp file, then rename atomically
    let tmp = tmp_download_path(dest);
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        pb.inc(chunk.len() as u64);
    }
    file.flush().await?;
    drop(file);

    pb.finish_and_clear();

    publish_downloaded_artifact(&tmp, dest).await?;
    Ok(())
}

/// Download a file silently (no progress bar).
#[doc(hidden)]
pub async fn download_silent(url: &str, dest: &Path) -> Result<()> {
    match try_parallel_download(url, dest, false).await {
        Ok(()) => return Ok(()),
        Err(e) => {
            tracing::debug!("parallel download failed, falling back to single connection: {e}")
        }
    }

    let client = asset_client()?;
    let resp = client.get(url).send().await?;

    if !resp.status().is_success() {
        anyhow::bail!("Download failed: HTTP {}", resp.status());
    }

    let tmp = tmp_download_path(dest);
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);

    publish_downloaded_artifact(&tmp, dest).await?;
    Ok(())
}

/// Fetch a small text asset (the SHA256SUMS manifest) into memory.
async fn fetch_asset_text(url: &str) -> Result<String> {
    let client = asset_client()?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("Download failed: HTTP {} for {}", resp.status(), url);
    }
    Ok(resp.text().await?)
}

/// Hex-encoded SHA-256 of a file, computed off the async runtime.
async fn sha256_hex_of_file(path: &Path) -> Result<String> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || -> Result<String> {
        use sha2::{Digest, Sha256};
        let mut f = std::fs::File::open(&path)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut f, &mut hasher)?;
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await
    .map_err(|e| anyhow::anyhow!("sha256 task panicked: {e}"))?
}

/// Look up the expected SHA-256 for `asset_name` in a `sha256sum`-format
/// manifest (`<hex><whitespace><name>` per line; a leading `*` on the name
/// marks binary mode and is ignored).
fn expected_sha256_for(sums: &str, asset_name: &str) -> Result<String> {
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((hash, name)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if name.trim().trim_start_matches('*') != asset_name {
            continue;
        }
        let hash = hash.trim();
        if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(hash.to_ascii_lowercase());
        }
        anyhow::bail!("malformed SHA256SUMS entry for {asset_name}: '{line}'");
    }
    anyhow::bail!("SHA256SUMS has no entry for {asset_name}")
}

/// Extract the single `kigi` binary from a release archive to `dest_tmp`
/// (caller publishes it with the usual chmod+rename). Archives are tar.gz on
/// Unix and zip on Windows, containing `kigi(.exe)` plus license files.
async fn extract_kigi_binary(archive: &Path, dest_tmp: &Path) -> Result<()> {
    let archive = archive.to_owned();
    let dest_tmp = dest_tmp.to_owned();
    tokio::task::spawn_blocking(move || extract_kigi_binary_blocking(&archive, &dest_tmp))
        .await
        .map_err(|e| anyhow::anyhow!("archive extraction task panicked: {e}"))?
}

#[cfg(not(windows))]
fn extract_kigi_binary_blocking(archive: &Path, dest_tmp: &Path) -> Result<()> {
    let f = std::fs::File::open(archive)?;
    let gz = flate2::read::GzDecoder::new(f);
    let mut ar = tar::Archive::new(gz);
    for entry in ar.entries()? {
        let mut entry = entry?;
        let is_kigi = entry
            .path()?
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "kigi");
        if is_kigi {
            let mut out = std::fs::File::create(dest_tmp)?;
            std::io::copy(&mut entry, &mut out)?;
            return Ok(());
        }
    }
    anyhow::bail!("archive {} contains no 'kigi' binary", archive.display())
}

#[cfg(windows)]
fn extract_kigi_binary_blocking(archive: &Path, dest_tmp: &Path) -> Result<()> {
    let f = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(f)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let base_name = entry
            .name()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_string();
        if base_name.eq_ignore_ascii_case("kigi.exe") {
            let mut out = std::fs::File::create(dest_tmp)?;
            std::io::copy(&mut entry, &mut out)?;
            return Ok(());
        }
    }
    anyhow::bail!(
        "archive {} contains no 'kigi.exe' binary",
        archive.display()
    )
}

/// Delete `~/.kigi/models_cache.json` after a successful update.
///
/// The cache embeds the binary version and will be treated as a miss by the
/// new binary anyway, but removing it eagerly avoids a wasted disk read +
/// deserialize on first launch.
async fn remove_stale_models_cache() {
    let cache = kigi_home().join("models_cache.json");
    match tokio::fs::remove_file(&cache).await {
        Ok(()) => tracing::debug!("removed stale models_cache.json after update"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::debug!("failed to remove stale models cache: {e}"),
    }
}

/// Remove stale links/binaries from `~/.kigi/bin/` left by installations
/// that predate the single-binary distribution rewrite (`agent`). `kigi`
/// is the single managed entry point now.
async fn remove_legacy_links(bin_dir: &Path) {
    let name = if cfg!(windows) { "agent.exe" } else { "agent" };
    let link = bin_dir.join(name);
    if link.exists() || link.is_symlink() {
        let _ = tokio::fs::remove_file(&link).await;
    }
}

async fn install_internal(target: Option<&str>, update_config: &UpdateConfig) -> Result<()> {
    install_internal_from_base(target, update_config, &crate::version::update_base_url()).await
}

/// Test-visible entry point: same as [`install_internal`] but resolves
/// releases from `base_url` (a GitHub-Releases-shaped API) instead of
/// [`kigi_env::update_base_url`]. Persists installer config and writes to
/// `~/.kigi/bin/`, so callers must isolate `KIGI_SHARE_DIR`.
#[doc(hidden)]
pub async fn install_internal_from_base(
    target: Option<&str>,
    update_config: &UpdateConfig,
    base_url: &str,
) -> Result<()> {
    let download = download_verified_from_base(target, update_config, base_url).await?;
    activate_verified_download(&download).await
}

const SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(10);

async fn smoke_test_binary(binary_path: &Path) -> bool {
    let mut cmd = tokio::process::Command::new(binary_path);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    kigi_tools::util::detach_command(&mut cmd);
    match tokio::time::timeout(SMOKE_TEST_TIMEOUT, cmd.status()).await {
        Ok(Ok(status)) => status.success(),
        _ => false,
    }
}

/// A downloaded, checksum-verified, and smoke-tested binary in
/// `~/.kigi/downloads/`, not yet activated as the managed `kigi`.
struct VerifiedDownload {
    version: String,
    binary_path: PathBuf,
}

/// Resolve the release to install: pinned version → `GET {base}/tags/v{v}`;
/// otherwise the channel's latest.
async fn resolve_release(target: Option<&str>, channel: &str, base_url: &str) -> Result<Release> {
    match target {
        Some(v) => {
            semver::Version::parse(v)
                .map_err(|_| anyhow::anyhow!("invalid version format: '{}'", v))?;
            crate::version::fetch_release_for_version_from_base(v, base_url).await
        }
        None => crate::version::fetch_latest_release_from_base(channel, base_url).await,
    }
}

/// Download phase: resolve the release, download the platform archive,
/// verify its SHA-256 against the release's SHA256SUMS manifest, extract the
/// `kigi` binary, and smoke-test it. Nothing is activated yet.
async fn download_verified_from_base(
    target: Option<&str>,
    update_config: &UpdateConfig,
    base_url: &str,
) -> Result<VerifiedDownload> {
    let (os, arch) = detect_platform()?;
    let platform = format!("{}-{}", os, arch);

    let release = resolve_release(target, &update_config.channel, base_url).await?;
    let version = release.version()?;

    let asset_name = release_asset_name(&version)?;
    let archive_asset = release.asset(&asset_name)?;
    let sums_asset = release.asset(SHA256SUMS_ASSET)?;

    // Fetch the checksum manifest FIRST: if it's missing or unreadable we
    // fail before spending bandwidth on the archive.
    let sums = fetch_asset_text(&sums_asset.browser_download_url).await?;
    let expected = expected_sha256_for(&sums, &asset_name)?;

    let kigi_home = kigi_home();
    let download_dir = kigi_home.join("downloads");
    tokio::fs::create_dir_all(&download_dir).await?;

    let archive_path = download_dir.join(&asset_name);
    let binary_name = format!("kigi-{}-{}", version, platform);
    let binary_path = download_dir.join(&binary_name);

    eprintln!("  Downloading kigi v{} ({})...", version, target_triple()?);

    download_with_progress(&archive_asset.browser_download_url, &archive_path).await?;

    // Checksum gate: a corrupt or tampered archive is deleted and never
    // extracted, let alone activated.
    let actual = sha256_hex_of_file(&archive_path).await?;
    if actual != expected {
        let _ = tokio::fs::remove_file(&archive_path).await;
        anyhow::bail!(
            "SHA256 mismatch for {asset_name}: expected {expected}, got {actual}.\n\
             Your current version is unchanged."
        );
    }

    // Extract to a unique temp sibling, then publish (chmod +x, atomic
    // rename) so a concurrent same-version installer never sees a partial
    // or non-executable binary at the final path.
    let extract_tmp = unique_temp_sibling(&binary_path, "tmp");
    if let Err(e) = extract_kigi_binary(&archive_path, &extract_tmp).await {
        let _ = tokio::fs::remove_file(&extract_tmp).await;
        let _ = tokio::fs::remove_file(&archive_path).await;
        return Err(e);
    }
    publish_downloaded_artifact(&extract_tmp, &binary_path).await?;

    // The archive has served its purpose; the versioned binary is what
    // `cleanup_old_downloads` retention manages.
    let _ = tokio::fs::remove_file(&archive_path).await;

    // Smoke-test: run the binary before activating it. A truncated or
    // corrupt extraction is caught here and never becomes the active kigi.
    if !smoke_test_binary(&binary_path).await {
        let _ = tokio::fs::remove_file(&binary_path).await;
        // No prefix: run_install_script's wrap adds "Auto-update failed:".
        anyhow::bail!(
            "downloaded binary failed to run.\n\
             Your current version is unchanged.\n\
             To update manually: {}",
            manual_install_cmd()
        );
    }

    Ok(VerifiedDownload {
        version,
        binary_path,
    })
}

/// Local activation phase: swap the managed `~/.kigi/bin/kigi` link to the
/// downloaded binary and finish bookkeeping.
async fn activate_verified_download(download: &VerifiedDownload) -> Result<()> {
    let kigi_home = kigi_home();
    let download_dir = kigi_home.join("downloads");
    let bin_dir = kigi_home.join("bin");
    tokio::fs::create_dir_all(&bin_dir).await?;

    // Atomic swap of ~/.kigi/bin/kigi -> downloaded binary.
    let link_path = swap_managed_bin_link(&download.binary_path, &bin_dir).await?;

    remove_legacy_links(&bin_dir).await;

    eprintln!();

    // Clean up old versioned binaries (keeps current + 1 previous).
    cleanup_old_downloads(&download_dir, "kigi", &download.version).await;

    // Persist installer to config.toml so future runs auto-detect internal.
    let _ = config::update_config(|st| {
        st.cli.installer = Some("internal".to_string());
    })
    .await;

    // Regenerate shell completions so they reflect the new binary's CLI surface.
    // Best-effort: failures are silently ignored (same as the installer).
    regenerate_completions(&link_path, &kigi_home).await;

    Ok(())
}

/// Regenerate shell completions after a binary update (best-effort).
///
/// Spawns the newly-installed binary with `completions <shell>` for each
/// supported shell and writes the output to the standard completion paths.
/// Failures are silently ignored — completions are a nice-to-have, not a
/// requirement for a successful update.
async fn regenerate_completions(binary: &Path, kigi_home: &Path) {
    // Derive $HOME independently — kigi_home may be overridden via KIGI_SHARE_DIR
    // env var, so kigi_home.parent() isn't necessarily the user's home dir.
    #[allow(deprecated)]
    let user_home = std::env::home_dir().unwrap_or_default();

    let completions: &[(&str, PathBuf)] = &[
        ("bash", kigi_home.join("completions/bash/kigi.bash")),
        ("zsh", kigi_home.join("completions/zsh/_kigi")),
        ("fish", user_home.join(".config/fish/completions/kigi.fish")),
    ];

    for (shell, dest) in completions {
        if let Some(parent) = dest.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let mut cmd = tokio::process::Command::new(binary);
        cmd.args(["completions", shell])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        kigi_tools::util::detach_command(&mut cmd);
        let Ok(output) = cmd.output().await else {
            continue;
        };
        if output.status.success() && !output.stdout.is_empty() {
            let _ = tokio::fs::write(dest, &output.stdout).await;
        }
    }
}

/// Compute a relative symlink target from `link` to `target`.
///
/// When both paths share a grandparent (e.g. `~/.kigi/bin/kigi` and
/// `~/.kigi/downloads/kigi-0.1.2-linux-x86_64`), returns a relative path
/// like `../downloads/kigi-0.1.2-linux-x86_64`.  When they share the same
/// parent directory, returns just the filename.  Falls back to the absolute
/// `target` path for any other layout.
///
/// Relative symlinks survive Docker bind-mounts where `~/.kigi/` is mapped
/// into a container with a different `$HOME` (and thus a different absolute
/// prefix).
#[cfg(unix)]
fn relative_symlink_target(target: &Path, link: &Path) -> PathBuf {
    let (Some(target_parent), Some(link_parent)) = (target.parent(), link.parent()) else {
        return target.to_path_buf();
    };
    // Same directory — just the filename (e.g. kigi-latest -> kigi-0.1.2-…)
    if target_parent == link_parent
        && let Some(name) = target.file_name()
    {
        return PathBuf::from(name);
    }
    // Sibling directories — ../target_dir/filename (e.g. bin/kigi -> ../downloads/kigi-…)
    if let (Some(tp), Some(lp)) = (target_parent.parent(), link_parent.parent())
        && tp == lp
        && let (Some(dir_name), Some(file_name)) = (target_parent.file_name(), target.file_name())
    {
        return Path::new("..").join(dir_name).join(file_name);
    }
    target.to_path_buf()
}

/// Swap `~/.kigi/bin/kigi` to point at `binary_path`. Returns the link path
/// (for [`regenerate_completions`]).
///
/// Unix: atomic symlink swap with relative target (survives Docker
/// bind-mounts of `~/.kigi/`); a failed swap leaves the prior link intact.
/// Windows: [`windows_replace_exe`], which restores the prior binary itself
/// when the replacement copy fails.
async fn swap_managed_bin_link(binary_path: &Path, bin_dir: &Path) -> Result<PathBuf> {
    let kigi_name = if cfg!(windows) { "kigi.exe" } else { "kigi" };
    let link_path = bin_dir.join(kigi_name);

    #[cfg(unix)]
    {
        let rel_target = relative_symlink_target(binary_path, &link_path);
        atomic_symlink_swap(&rel_target, &link_path)
            .await
            .with_context(|| format!("swapping managed bin link {}", link_path.display()))?;
    }
    #[cfg(windows)]
    {
        windows_replace_exe(binary_path, &link_path)
            .await
            .with_context(|| format!("replacing managed binary {}", link_path.display()))?;
    }
    #[cfg(not(any(unix, windows)))]
    {
        // No managed bin layout on this target; no-op.
        let _ = binary_path;
    }

    Ok(link_path)
}

/// Atomically swap a symlink to point to a new target.
///
/// Creates a temporary symlink next to `link_path`, then renames it over the
/// old symlink.  This avoids the remove-then-create race where the path
/// briefly doesn't exist, and — crucially — never deletes the old target
/// file.  On macOS (especially Apple Silicon), deleting a binary that a
/// running process has mmap'd causes SIGKILL because the kernel can no longer
/// verify the code signature of the executable pages.
#[cfg(unix)]
async fn atomic_symlink_swap(target: &Path, link_path: &Path) -> Result<()> {
    // Per-racer temp name: a shared one makes remove_file → symlink racy
    // (EEXIST, or ENOENT when another racer renames the link away).
    sweep_stale_tmp_links(link_path, STALE_TMP_AGE).await;
    let tmp_link = unique_temp_sibling(link_path, "tmp-link");
    let _ = tokio::fs::remove_file(&tmp_link).await;
    tokio::fs::symlink(target, &tmp_link).await?;
    tokio::fs::rename(&tmp_link, link_path).await?;
    Ok(())
}

/// Remove `<link>.*.tmp-link` siblings left by a swap that crashed between
/// symlink and rename. Only those older than `max_age` are removed, so a
/// concurrent racer's in-flight link is never deleted out from under it.
#[cfg(unix)]
async fn sweep_stale_tmp_links(link_path: &Path, max_age: Duration) {
    let (Some(dir), Some(name)) = (
        link_path.parent(),
        link_path.file_name().and_then(|n| n.to_str()),
    ) else {
        return;
    };
    let prefix = format!("{name}.");
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let fname = entry.file_name();
        let Some(fname) = fname.to_str() else {
            continue;
        };
        if !fname.starts_with(&prefix) || !fname.ends_with(".tmp-link") {
            continue;
        }
        let stale = tokio::fs::symlink_metadata(entry.path())
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > max_age);
        if stale {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// Replace an executable that may be locked by a running process (Windows).
///
/// On Windows the kernel prevents writes to a running executable but allows
/// renames. If a direct copy fails with a sharing violation, this renames
/// `dest` aside and copies `src` into the freed path. If the copy then
/// fails, the rename is rolled back to avoid a broken install.
///
/// The aside target is normally `<dest>.old`, but a leftover `.old` can
/// itself still be a running image (the session that was live during the
/// previous update keeps executing the renamed-aside file), and a running
/// image can neither be deleted nor rename-replaced. In that case `dest` is
/// renamed to a unique `<dest>.old.{pid}-{seq}.old` sibling instead, so a
/// locked leftover can never block the update. All `.old` leftovers are
/// swept best-effort at the start of each cycle; still-locked ones survive
/// until a later update runs after those processes exit.
#[cfg(windows)]
async fn windows_replace_exe(src: &Path, dest: &Path) -> Result<()> {
    let file_name = dest
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("destination has no filename: {}", dest.display()))?
        .to_string_lossy();
    let old = dest.with_file_name(format!("{file_name}.old"));

    sweep_old_exe_backups(&old).await;

    match tokio::fs::copy(src, dest).await {
        Ok(_) => return Ok(()),
        // ERROR_SHARING_VIOLATION (32) / ERROR_ACCESS_DENIED (5): exe is
        // locked by a running process. Fall through to rename-and-replace.
        Err(e) if matches!(e.raw_os_error(), Some(32) | Some(5)) => {
            tracing::debug!("exe locked, falling back to rename: {e}");
        }
        Err(e) => return Err(e.into()),
    }

    // A .old that survived the sweep is locked; renaming onto it would need
    // to delete-replace it and fail, so divert to a guaranteed-free name.
    let old_is_free = matches!(
        tokio::fs::symlink_metadata(&old).await,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    );
    let mut aside = if old_is_free {
        old.clone()
    } else {
        let diverted = unique_temp_sibling(&old, "old");
        tracing::debug!(
            "stale {} is locked; diverting aside to {}",
            old.display(),
            diverted.display()
        );
        diverted
    };

    // Move the locked file aside, then copy the new binary into place.
    let mut rename_result = tokio::fs::rename(dest, &aside).await;
    // Pid reuse can collide a diverted name with a dead updater's
    // still-locked leftover, and a racer can occupy a just-checked-free
    // .old; a fresh unique sibling clears both tails (3 attempts total).
    for _ in 0..2 {
        match &rename_result {
            Err(e) if matches!(e.raw_os_error(), Some(32) | Some(5)) => {
                tracing::debug!(
                    "rename aside to {} failed; retrying with a fresh name: {e}",
                    aside.display()
                );
                aside = unique_temp_sibling(&old, "old");
                rename_result = tokio::fs::rename(dest, &aside).await;
            }
            _ => break,
        }
    }
    rename_result.map_err(|e| {
        anyhow::anyhow!(
            "cannot rename locked executable {}: {e}\n\
             Close all running kigi sessions and retry.",
            dest.display(),
        )
    })?;
    match tokio::fs::copy(src, dest).await {
        Ok(_) => Ok(()),
        Err(e) => {
            // Rollback: restore the old binary so the install isn't broken.
            let _ = tokio::fs::rename(&aside, dest).await;
            Err(e.into())
        }
    }
}

/// Best-effort removal of `<exe>.old` plus the unique
/// `<exe>.old.{pid}-{seq}.old` asides accumulated by prior update cycles.
/// Locked ones (still-running images) survive and are collected by a later
/// update once those processes exit. The `<exe>.old` prefix keeps the sweep
/// away from `<exe>` itself, other executables' leftovers, and the `.tmp`
/// sibling shapes.
///
/// Unlike `sweep_stale_tmp_links` there is deliberately no `max_age` gate:
/// rename preserves mtime, so a racer's seconds-old aside already looks
/// days old and age cannot distinguish it; in-use asides survive deletion
/// by being locked; and deleting a racer's fresh unlocked aside (its
/// rollback source while both racers converge on the same dest) is the
/// accepted lock-free residual race (see `tmp_download_path`).
#[cfg(windows)]
async fn sweep_old_exe_backups(old: &Path) {
    let _ = tokio::fs::remove_file(old).await;
    let (Some(dir), Some(old_name)) = (old.parent(), old.file_name().and_then(|n| n.to_str()))
    else {
        return;
    };
    let prefix = format!("{old_name}.");
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(&prefix) && name.ends_with(".old") {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// Best-effort cleanup of old versioned binaries for a given binary name.
///
/// Keeps the current version plus one previous version (in case a process is
/// still running the old binary and hasn't fully loaded all pages yet —
/// deleting it on macOS causes SIGKILL because the kernel can no longer
/// verify the code signature).
///
/// `bin_prefix` is the binary name prefix, e.g. `"kigi"`. Files must match
/// `{bin_prefix}-{digit}*` to be considered versioned binaries (this avoids
/// `kigi-*` matching `kigi-latest` or differently-suffixed siblings).
///
/// Temporary/partial files (containing `.tmp`) are deleted only once they
/// are **stale** (mtime older than [`STALE_TMP_AGE`]). A fresh `.tmp` may be
/// a concurrent updater's in-flight download — the same-instant race the
/// lock-free design accepts — and deleting it out from under that updater
/// would make its atomic rename fail.
async fn cleanup_old_downloads(dir: &Path, bin_prefix: &str, current_version: &str) {
    let prefix = format!("{}-", bin_prefix);
    let current_semver = match semver::Version::parse(current_version) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "cleanup_old_downloads: invalid current version '{}': {}",
                current_version,
                e
            );
            return;
        }
    };

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!(
                "cleanup_old_downloads: failed to read {}: {}",
                dir.display(),
                e
            );
            return;
        }
    };

    let mut versioned: Vec<(semver::Version, String)> = Vec::new();

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix) {
            continue;
        }
        // Temp/partial files: sweep only STALE ones. A fresh `.tmp` may be a
        // concurrent updater's in-flight download — deleting it would make
        // that updater's atomic rename fail with ENOENT.
        if name.contains(".tmp") {
            let stale = match entry.metadata().await.and_then(|m| m.modified()) {
                Ok(modified) => std::time::SystemTime::now()
                    .duration_since(modified)
                    .map(|age| age > STALE_TMP_AGE)
                    // Future mtime (clock skew): can't tell — leave it.
                    .unwrap_or(false),
                // Unknown mtime: leave it; it is swept once readable+old.
                Err(_) => false,
            };
            if stale && let Err(e) = tokio::fs::remove_file(entry.path()).await {
                tracing::warn!("failed to remove stale temp file {}: {}", name, e);
            }
            continue;
        }
        // Skip symlinks (e.g. kigi-latest).
        if let Ok(ft) = entry.file_type().await
            && ft.is_symlink()
        {
            continue;
        }
        // The suffix after the prefix must start with a digit to be a versioned
        // binary (avoids `kigi-latest` and other non-versioned siblings).
        let suffix = &name[prefix.len()..];
        if !suffix.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        // Extract the version portion via the shared parser (handles the
        // managed `kigi-0.1.0-macos-aarch64`, pre-release, and bare
        // `kigi-0.1.0` layouts — see `version_from_versioned_binary_name`).
        let Some(ver_str) = crate::version::version_from_versioned_binary_name(&name, bin_prefix)
        else {
            continue;
        };
        if let Ok(v) = semver::Version::parse(&ver_str) {
            // Skip the current version — never delete it.
            if v == current_semver {
                continue;
            }
            versioned.push((v, name));
        }
    }

    // Sort descending by version so the newest is first.
    versioned.sort_by(|a, b| b.0.cmp(&a.0));

    // Keep the most recent old version (index 0), delete the rest (index 1+).
    for (_, name) in versioned.iter().skip(1) {
        let path = dir.join(name);
        // Same freshness guard as the `.tmp` sweep: a versioned binary
        // written moments ago is likely a concurrent installer's
        // just-renamed download (its symlink swap hasn't happened yet) —
        // deleting it would leave that installer's swap pointing at
        // nothing. Old binaries from previous releases are days old.
        let fresh = tokio::fs::metadata(&path)
            .await
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age <= STALE_TMP_AGE);
        if fresh {
            continue;
        }
        if let Err(e) = tokio::fs::remove_file(&path).await {
            tracing::warn!("failed to remove old binary {}: {}", name, e);
        }
    }
}

pub async fn apply_channel_switch(channel_switch: Option<&str>, update_config: &mut UpdateConfig) {
    if let Some(ch) = channel_switch
        && update_config.channel != ch
    {
        let _ = config::update_config(|st| {
            st.cli.channel = Some(ch.to_string());
        })
        .await;
        update_config.channel = ch.to_string();
        eprintln!("Switched to {} channel.", ch);
    }
}

/// Run the `kigi update` command. Returns `Ok(Some(version))` when the target
/// version is present on disk afterwards — either installed by this call or
/// found already installed (e.g. by a concurrent background download); returns
/// `Ok(None)` when there is no installer or no applicable target. Callers use
/// the returned version to signal a running leader to relaunch onto the new
/// binary (see the pager's post-update leader relaunch) — that signal must
/// fire even when the download itself was skipped, so a stale leader still
/// picks up a binary someone else installed.
pub async fn run_update(
    force: bool,
    pinned_version: Option<&str>,
    channel_switch: Option<&str>,
    update_config: &mut UpdateConfig,
) -> Result<Option<String>> {
    apply_channel_switch(channel_switch, update_config).await;
    let installer = match get_installer().await {
        Some(i) => i,
        None => {
            eprintln!("Auto-update is not available for manual installations.");
            return Ok(None);
        }
    };

    // Persist installer if not already saved
    let cfg = config::load_config().await;
    if cfg.cli.installer.is_none() {
        let _ = config::update_config(|st| {
            st.cli.installer = Some(installer.to_string());
        })
        .await;
    }

    let current_version = get_installed_kigi_version();

    // When --version is given, skip the latest-version check and install directly
    if let Some(version) = pinned_version {
        if let Err(e) = crate::minimum_version::check_install_target(version) {
            anyhow::bail!("{e}");
        }
        eprintln!(
            "Installing Kigi {} (current: {})...",
            version, current_version
        );
        eprintln!();
        run_install_script(installer, Some(version), update_config).await?;
        refresh_deployment_config().await;
        if let Err(e) = config::update_config(|st| {
            st.cli.auto_update = Some(false);
        })
        .await
        {
            tracing::warn!("Failed to persist auto_update=false for pinned install: {e}");
        }
        eprintln!("  ✓ kigi v{} installed successfully!", version);
        eprintln!("  Please restart Kigi.");
        return Ok(Some(version.to_string()));
    }

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("  {spinner:.cyan} Checking for updates...")
            .unwrap(),
    );
    pb.enable_steady_tick(Duration::from_millis(100));
    let latest_version = fetch_latest_version(update_config).await?;
    pb.finish_and_clear();

    let install_target = match crate::minimum_version::apply_floor(&latest_version) {
        Ok(t) => t,
        Err(e) => anyhow::bail!("{e}"),
    };
    if install_target != latest_version {
        eprintln!(
            "Latest available is {} but the configured minimum is higher; \
             installing {} instead.",
            latest_version, install_target
        );
    }

    // What's on disk wins over this process's compiled-in version: a
    // concurrent or earlier updater (TUI background download, leader hourly
    // checker) may already have installed the target, in which case there is
    // nothing to download.
    let effective_current =
        disk_version_for_installer(installer).unwrap_or_else(|| current_version.clone());

    if !force {
        match needs_update(
            &effective_current,
            &install_target,
            &update_config.channel,
            installer_allows_downgrade(installer),
        ) {
            Some(true) => {}
            Some(false) => {
                // Explicit channel switch (--stable / --alpha) with a
                // different target version: install even though the current
                // version is "newer" by semver. This handles switching from
                // alpha 0.2.X back to stable 0.1.220 where 0.2.X > 0.1.220.
                if channel_switch.is_some() && effective_current != install_target {
                    // Fall through to install
                } else {
                    let stable_ptr = try_fetch_stable_version().await;
                    write_version_cache(&install_target, stable_ptr.as_deref()).await;
                    eprintln!("Already up to date ({}).", effective_current);
                    // Retry if a prior sync failed.
                    refresh_deployment_config().await;
                    // The target is on disk even though this call installed
                    // nothing — report it so the caller still signals stale
                    // leaders to relaunch onto it (signalling is directional
                    // and skips leaders already at/after this version).
                    return Ok(Some(install_target));
                }
            }
            None => {
                // Distinguish parse failure from unsupported channel.
                let parse_ok = semver::Version::parse(&effective_current).is_ok()
                    && semver::Version::parse(&install_target).is_ok();
                if parse_ok {
                    anyhow::bail!(
                        "Unsupported release channel '{}' (current={}, target={}). \
                         Supported channels: stable, alpha, enterprise. \
                         Use --stable or --alpha to override, or set [cli] channel in config.toml.",
                        update_config.channel,
                        effective_current,
                        install_target
                    );
                } else {
                    anyhow::bail!(
                        "Failed to parse versions (current={}, target={})",
                        effective_current,
                        install_target
                    );
                }
            }
        }
    }

    let target_version = if force
        && !needs_update(
            &effective_current,
            &install_target,
            &update_config.channel,
            installer_allows_downgrade(installer),
        )
        .unwrap_or(true)
    {
        eprintln!(
            "Forcing reinstall of Kigi {} (already up to date)",
            effective_current
        );
        &effective_current
    } else {
        eprintln!("Updating Kigi {} → {}", effective_current, install_target);
        &install_target
    };

    eprintln!();
    run_install_script(installer, Some(target_version), update_config).await?;
    // Fetch the stable version now so the new binary has it immediately
    // for channel_label() display, rather than waiting for the next
    // TTL-gated update check (~30 min).
    let stable_ptr = try_fetch_stable_version().await;
    write_version_cache(target_version, stable_ptr.as_deref()).await;
    refresh_deployment_config().await;
    eprintln!("  ✓ kigi v{} installed successfully!", target_version);

    if !force && std::env::var_os("KIGI_AUTO_UPDATE").is_none() {
        eprintln!("  Please restart Kigi.");
    }
    Ok(Some(target_version.to_string()))
}

/// Refresh managed config post-update (best-effort, staleness-gated), for
/// deployment-key and team principals alike.
async fn refresh_deployment_config() {
    if !kigi_shell::managed_config::has_principal() {
        return;
    }
    if !kigi_shell::managed_config::is_fetch_enabled() {
        return;
    }
    // Clear a logged-out team's files before deciding to fetch (mirrors the loop).
    kigi_shell::managed_config::clear_orphan();
    if !kigi_shell::config::is_managed_config_stale_for(
        &kigi_shell::managed_config::current_serving_identity(),
    ) {
        return;
    }
    match kigi_shell::managed_config::sync().await {
        Ok(true) => eprintln!("  Applied managed configuration."),
        Ok(false) => tracing::debug!("no managed configuration to apply"),
        // Auth issues aren't actionable mid-update: quiet here, loud on `kigi setup`.
        Err(e) if e.is_auth_rejection() => tracing::debug!("managed config not applied: {e}"),
        Err(e) if e.is_retryable() => {
            tracing::debug!("managed config refresh failed: {e}");
            eprintln!("  Couldn't apply managed configuration. Run `kigi setup` to retry.");
        }
        Err(e) => eprintln!("  Couldn't apply managed configuration. {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tmp_download_path_is_unique_per_version_and_per_attempt() {
        // `with_extension("tmp")` would collapse every 0.1.x versioned name
        // onto a single `kigi-0.1.tmp`; the helper must keep distinct
        // versions distinct AND make repeated attempts (same process, e.g.
        // concurrent tokio tasks) unique.
        let dest_181 = Path::new("/home/u/.kigi/downloads/kigi-0.1.181-linux-x86_64");
        let dest_182 = Path::new("/home/u/.kigi/downloads/kigi-0.1.182-linux-x86_64");

        let a = tmp_download_path(dest_181);
        let b = tmp_download_path(dest_182);
        assert_ne!(a, b, "different versions must not share a temp file");

        let a2 = tmp_download_path(dest_181);
        assert_ne!(
            a, a2,
            "two attempts for the same dest must not share a temp file"
        );

        let name = a.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("kigi-0.1.181-linux-x86_64."),
            "full versioned name must be preserved: {name}"
        );
        assert!(
            name.ends_with(".tmp") && name.contains(&std::process::id().to_string()),
            "temp name must embed the PID and end in .tmp (cleanup sweeps *.tmp*): {name}"
        );
        assert_eq!(
            a.parent(),
            Path::new("/home/u/.kigi/downloads").into(),
            "temp file must stay in the destination directory for atomic rename"
        );
    }

    // needs_update — channel/upgrade/downgrade semantics

    #[test]
    fn test_needs_update_matrix() {
        // (current, target, channel, allow_downgrade, expected)
        let cases: &[(&str, &str, &str, bool, Option<bool>)] = &[
            // Same version: never an update, regardless of allow_downgrade.
            ("0.1.141", "0.1.141", "stable", false, Some(false)),
            ("0.2.5", "0.2.5", "stable", true, Some(false)),
            ("0.2.5", "0.2.5", "alpha", true, Some(false)),
            // Plain upgrades.
            ("0.1.140", "0.1.141", "stable", false, Some(true)),
            ("0.2.5", "0.2.7", "stable", true, Some(true)),
            ("0.2.5", "0.2.7", "alpha", false, Some(true)),
            ("0.1.140", "0.1.999", "stable", false, Some(true)),
            ("0.1.999", "0.2.0", "stable", false, Some(true)),
            ("99.99.99", "100.0.0", "stable", false, Some(true)),
            ("0.0.0", "0.0.1", "stable", false, Some(true)),
            // Downgrades: only when the installer allows them.
            ("0.1.141", "0.1.140", "stable", false, Some(false)),
            ("0.2.7", "0.2.5", "stable", true, Some(true)),
            ("0.2.7", "0.2.5", "alpha", true, Some(true)),
            ("0.1.207", "0.1.206", "enterprise", true, Some(true)),
            ("2.0.0", "1.99.99", "stable", true, Some(true)),
            ("2.0.0", "1.99.99", "stable", false, Some(false)),
            // Pre-release targets rejected on stable/enterprise, even for
            // rollbacks.
            ("0.1.139", "0.1.140-alpha.1", "stable", false, Some(false)),
            ("0.2.7", "0.2.5-alpha.1", "stable", true, Some(false)),
            ("0.2.7", "0.2.5-alpha.1", "enterprise", true, Some(false)),
            (
                "0.1.150-alpha.1",
                "0.1.151-alpha.1",
                "stable",
                false,
                Some(false),
            ),
            // Pre-release CURRENT on stable/enterprise: force-install a
            // release even if semver-lower, independent of allow_downgrade.
            ("0.1.149-alpha.1", "0.1.148", "stable", false, Some(true)),
            ("0.1.149-alpha.1", "0.1.148", "stable", true, Some(true)),
            (
                "0.1.206-alpha.3",
                "0.1.206",
                "enterprise",
                false,
                Some(true),
            ),
            // Alpha channel follows raw semver.
            ("0.1.140-alpha.8", "0.1.140", "alpha", false, Some(true)),
            (
                "0.1.148-alpha.1",
                "0.1.148-alpha.3",
                "alpha",
                false,
                Some(true),
            ),
            (
                "0.1.148-alpha.3",
                "0.1.148-alpha.2",
                "alpha",
                false,
                Some(false),
            ),
            (
                "0.1.148-alpha.3",
                "0.1.148-alpha.2",
                "alpha",
                true,
                Some(true),
            ),
            ("0.1.150-alpha.99", "0.1.150", "alpha", false, Some(true)),
            (
                "0.1.150-alpha.5",
                "0.1.150-beta.1",
                "alpha",
                false,
                Some(true),
            ),
            ("0.1.140", "0.1.139-alpha.5", "alpha", false, Some(false)),
            // Enterprise behaves like stable for upgrades.
            ("0.1.205", "0.1.206", "enterprise", false, Some(true)),
            ("0.1.207", "0.1.206", "enterprise", false, Some(false)),
            (
                "0.1.205",
                "0.1.206-alpha.1",
                "enterprise",
                false,
                Some(false),
            ),
            // Parse failures and unknown channels → None.
            ("not-a-version", "0.1.141", "stable", false, None),
            ("0.1.141", "garbage", "stable", false, None),
            ("garbage", "0.1.141", "alpha", false, None),
            ("", "0.1.141", "stable", false, None),
            ("0.1.141", "", "stable", false, None),
            ("  0.1.141", "0.1.142", "stable", false, None),
            ("0.1", "0.1.141", "stable", false, None),
            ("0.1.140", "0.1.141", "beta", false, None),
            ("0.1.140", "0.1.141", "beta", true, None),
            ("0.1.140", "0.1.141", "STABLE", false, None),
            ("0.1.140", "0.1.141", "Stable", false, None),
            ("0.1.140", "0.1.141", "", false, None),
        ];
        for (current, target, channel, allow_downgrade, expected) in cases {
            assert_eq!(
                needs_update(current, target, channel, *allow_downgrade),
                *expected,
                "needs_update({current:?}, {target:?}, {channel:?}, {allow_downgrade})"
            );
        }
    }

    #[test]
    fn test_needs_update_with_build_metadata_uses_semver_crate_ordering() {
        // SUBTLE: per the semver SPEC, build metadata (after `+`) MUST be
        // ignored when determining version precedence. However the `semver`
        // crate's `PartialOrd` impl compares build metadata lexicographically
        // for differing values. So `0.1.141+xyz > 0.1.141+abc` returns true
        // here even though spec-wise they are equal.
        //
        // This means the release pipeline MUST NOT publish multiple builds of
        // the same version differing only in build metadata, or auto-update
        // will bounce users between them. The test locks in the surprising
        // behavior so it can't change silently.
        assert_eq!(
            needs_update("0.1.141+abc", "0.1.141+xyz", "stable", false),
            Some(true),
            "semver crate orders by build metadata lexicographically (contra spec)"
        );
        assert_eq!(
            needs_update("0.1.141", "0.1.141+abc", "stable", false),
            Some(true)
        );
    }

    // installer_allows_downgrade

    #[test]
    fn test_installer_allows_downgrade_internal_only() {
        assert!(installer_allows_downgrade("internal"));
        assert!(!installer_allows_downgrade("unknown"));
        assert!(!installer_allows_downgrade(""));
        assert!(!installer_allows_downgrade("npm"));
        assert!(!installer_allows_downgrade("homebrew"));
    }

    // atomic_symlink_swap

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_creates_new_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("binary-v1");
        std::fs::write(&target, "v1").unwrap();

        let link = dir.path().join("kigi");
        // No existing symlink — should create one.
        atomic_symlink_swap(&target, &link).await.unwrap();

        assert!(link.is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "v1");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_replaces_existing_and_preserves_old_target() {
        let dir = tempfile::tempdir().unwrap();

        let target_v1 = dir.path().join("binary-v1");
        std::fs::write(&target_v1, "v1-content").unwrap();
        let target_v2 = dir.path().join("binary-v2");
        std::fs::write(&target_v2, "v2-content").unwrap();

        let link = dir.path().join("kigi");
        std::os::unix::fs::symlink(&target_v1, &link).unwrap();
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "v1-content");

        atomic_symlink_swap(&target_v2, &link).await.unwrap();

        assert!(link.is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), target_v2);
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "v2-content");

        // The old target file must still exist on disk — this is the key
        // property that prevents SIGKILL on macOS.  Running processes that
        // have binary-v1 mmap'd can continue to page-fault from it.
        assert!(target_v1.exists(), "old binary must not be deleted");
        assert_eq!(std::fs::read_to_string(&target_v1).unwrap(), "v1-content");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_replaces_regular_file() {
        // If the canonical path is a regular file (from an old non-symlink
        // installation), the swap should still work by replacing it.
        let dir = tempfile::tempdir().unwrap();

        let target = dir.path().join("binary-v2");
        std::fs::write(&target, "v2").unwrap();

        let link = dir.path().join("kigi");
        // Simulate an old installation where kigi is a regular file.
        std::fs::write(&link, "old-binary").unwrap();

        atomic_symlink_swap(&target, &link).await.unwrap();

        assert!(link.is_symlink());
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "v2");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_succeeds_despite_leftover_tmp_link() {
        // A leftover .tmp-link from a crashed swap must not block a new swap:
        // unique per-racer temp names mean no collision.
        let dir = tempfile::tempdir().unwrap();

        let target_v1 = dir.path().join("binary-v1");
        std::fs::write(&target_v1, "v1").unwrap();
        let target_v2 = dir.path().join("binary-v2");
        std::fs::write(&target_v2, "v2").unwrap();

        let link = dir.path().join("kigi");
        std::os::unix::fs::symlink(&target_v1, &link).unwrap();
        std::os::unix::fs::symlink(&target_v1, link.with_extension("tmp-link")).unwrap();

        atomic_symlink_swap(&target_v2, &link).await.unwrap();

        assert_eq!(std::fs::read_to_string(&link).unwrap(), "v2");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_sweep_stale_tmp_links_removes_stale_keeps_fresh_and_active() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("binary-v1");
        std::fs::write(&target, "v1").unwrap();
        let link = dir.path().join("kigi");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Old- and new-style leftover temp links.
        let leftover_old = dir.path().join("kigi.tmp-link");
        let leftover_new = dir.path().join("kigi.123-0.tmp-link");
        std::os::unix::fs::symlink(&target, &leftover_old).unwrap();
        std::os::unix::fs::symlink(&target, &leftover_new).unwrap();

        // max_age = ZERO: every leftover is stale and removed; the active
        // `kigi` link (no `.tmp-link` suffix) is untouched.
        sweep_stale_tmp_links(&link, Duration::ZERO).await;
        assert!(!leftover_old.exists() && !leftover_new.exists());
        assert!(link.is_symlink(), "active link must be preserved");

        // A fresh leftover under a real max_age is preserved — it could be a
        // concurrent racer's in-flight link.
        let fresh = dir.path().join("kigi.999-9.tmp-link");
        std::os::unix::fs::symlink(&target, &fresh).unwrap();
        sweep_stale_tmp_links(&link, Duration::from_secs(3600)).await;
        assert!(fresh.exists(), "fresh tmp-link must be preserved");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_multiple_sequential_swaps() {
        // Simulate v1 -> v2 -> v3 -> v4 sequential swaps.
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("kigi");

        for i in 1..=4 {
            let target = dir.path().join(format!("binary-v{}", i));
            std::fs::write(&target, format!("content-v{}", i)).unwrap();
            atomic_symlink_swap(&target, &link).await.unwrap();

            assert!(link.is_symlink());
            assert_eq!(
                std::fs::read_to_string(&link).unwrap(),
                format!("content-v{}", i)
            );
        }

        // All old binaries should still be on disk.
        for i in 1..=4 {
            let target = dir.path().join(format!("binary-v{}", i));
            assert!(target.exists(), "binary-v{} should still exist", i);
        }

        // No temp files should remain.
        let tmp_link = link.with_extension("tmp-link");
        assert!(!tmp_link.exists(), "no temp link should remain");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_broken_symlink_target() {
        // If the current symlink is broken (target deleted externally),
        // the swap should still succeed.
        let dir = tempfile::tempdir().unwrap();

        let link = dir.path().join("kigi");
        // Create a broken symlink — points to a file that doesn't exist.
        std::os::unix::fs::symlink(dir.path().join("deleted-binary"), &link).unwrap();
        assert!(link.is_symlink());
        assert!(!link.exists(), "broken symlink should not 'exist'");

        // New target to swap to.
        let target = dir.path().join("binary-v2");
        std::fs::write(&target, "v2").unwrap();

        atomic_symlink_swap(&target, &link).await.unwrap();

        assert!(link.is_symlink());
        assert!(link.exists(), "symlink should now resolve");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "v2");
    }

    #[cfg(unix)]
    #[test]
    fn test_relative_symlink_target_layouts() {
        // bin/kigi -> ../downloads/kigi-0.1.2 (sibling directories)
        let target = Path::new("/home/alice/.kigi/downloads/kigi-0.1.2");
        let link = Path::new("/home/alice/.kigi/bin/kigi");
        assert_eq!(
            relative_symlink_target(target, link),
            PathBuf::from("../downloads/kigi-0.1.2")
        );

        // downloads/kigi-latest -> kigi-0.1.2 (same directory)
        let link = Path::new("/home/alice/.kigi/downloads/kigi-latest");
        assert_eq!(
            relative_symlink_target(target, link),
            PathBuf::from("kigi-0.1.2")
        );

        // /usr/local/bin/kigi -> absolute (different grandparents)
        let link = Path::new("/usr/local/bin/kigi");
        assert_eq!(
            relative_symlink_target(target, link),
            PathBuf::from("/home/alice/.kigi/downloads/kigi-0.1.2")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_relative_symlink_survives_directory_move() {
        // Simulates Docker bind-mount: create ~/.kigi/ layout at path A,
        // then copy it to path B and verify the symlink still resolves.
        let dir = tempfile::tempdir().unwrap();

        let alice = dir.path().join("alice").join(".kigi");
        let alice_downloads = alice.join("downloads");
        let alice_bin = alice.join("bin");
        std::fs::create_dir_all(&alice_downloads).unwrap();
        std::fs::create_dir_all(&alice_bin).unwrap();
        std::fs::write(alice_downloads.join("kigi-0.1.2"), "binary-content").unwrap();

        let rel_target = Path::new("../downloads/kigi-0.1.2");
        let link = alice_bin.join("kigi");
        atomic_symlink_swap(rel_target, &link).await.unwrap();
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "binary-content");

        // "Bind-mount" to bob: copy the entire .kigi tree.
        let bob_home = dir.path().join("bob");
        std::fs::create_dir_all(&bob_home).unwrap();
        let bob = bob_home.join(".kigi");
        let copy_status = std::process::Command::new("cp")
            .args(["-a", alice.to_str().unwrap(), bob.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(copy_status.success());

        let bob_link = bob.join("bin").join("kigi");
        assert!(bob_link.is_symlink());
        assert_eq!(
            std::fs::read_link(&bob_link).unwrap(),
            PathBuf::from("../downloads/kigi-0.1.2"),
            "symlink target should be relative"
        );
        assert_eq!(
            std::fs::read_to_string(&bob_link).unwrap(),
            "binary-content",
            "relative symlink should resolve at the new path"
        );
    }

    // cleanup_old_downloads

    /// Backdate a file's mtime past [`STALE_TMP_AGE`] so cleanup treats it
    /// as an abandoned download / genuinely old binary.
    fn make_stale(path: &Path) {
        let old = std::time::SystemTime::now() - (STALE_TMP_AGE + Duration::from_secs(60));
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
    }

    /// Backdate every file in `dir`. Cleanup deliberately never deletes a
    /// freshly-written binary or temp file (it may belong to a concurrent
    /// in-flight install), so retention-policy tests must age their fixtures
    /// to look like real leftovers from previous releases.
    fn make_all_stale(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_file() {
                make_stale(&p);
            }
        }
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_keeps_current_plus_one() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        for v in ["0.1.140", "0.1.141", "0.1.142", "0.1.143", "0.1.144"] {
            std::fs::write(d.join(format!("kigi-{}-macos-aarch64", v)), v).unwrap();
        }
        std::fs::write(d.join("kigi-0.1.145-macos-aarch64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "kigi", "0.1.145").await;

        assert!(d.join("kigi-0.1.145-macos-aarch64").exists(), "current");
        assert!(d.join("kigi-0.1.144-macos-aarch64").exists(), "N-1");
        for v in ["0.1.140", "0.1.141", "0.1.142", "0.1.143"] {
            assert!(
                !d.join(format!("kigi-{}-macos-aarch64", v)).exists(),
                "{v} should be deleted"
            );
        }
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_removes_stale_tmp_keeps_fresh_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Stale tmp: abandoned by a crashed updater — swept.
        std::fs::write(d.join("kigi-0.1.140-macos-aarch64.tmp"), "partial").unwrap();
        make_stale(&d.join("kigi-0.1.140-macos-aarch64.tmp"));
        // Fresh tmp: a concurrent updater's in-flight download — kept, or
        // its atomic rename would fail with ENOENT.
        std::fs::write(d.join("kigi-0.1.142-macos-aarch64.77-0.tmp"), "inflight").unwrap();
        std::fs::write(d.join("kigi-0.1.141-macos-aarch64"), "current").unwrap();

        cleanup_old_downloads(d, "kigi", "0.1.141").await;

        assert!(
            !d.join("kigi-0.1.140-macos-aarch64.tmp").exists(),
            "stale tmp cleaned up"
        );
        assert!(
            d.join("kigi-0.1.142-macos-aarch64.77-0.tmp").exists(),
            "fresh in-flight tmp must NOT be swept"
        );
        assert!(
            d.join("kigi-0.1.141-macos-aarch64").exists(),
            "current kept"
        );
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_keeps_fresh_versioned_binary() {
        // A versioned binary written moments ago may be a concurrent
        // installer's just-renamed download whose symlink swap hasn't
        // happened yet — even when the retention policy would otherwise
        // delete it, it must survive until it ages.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        for v in ["0.1.138", "0.1.139", "0.1.140"] {
            std::fs::write(d.join(format!("kigi-{v}-macos-aarch64")), v).unwrap();
        }
        std::fs::write(d.join("kigi-0.1.141-macos-aarch64"), "current").unwrap();
        make_all_stale(d);
        // .138 is re-written NOW — simulating a racer that just renamed its
        // download into place (e.g. a rollback install racing an upgrade).
        std::fs::write(d.join("kigi-0.1.138-macos-aarch64"), "in-flight").unwrap();

        cleanup_old_downloads(d, "kigi", "0.1.141").await;

        assert!(d.join("kigi-0.1.141-macos-aarch64").exists(), "current");
        assert!(d.join("kigi-0.1.140-macos-aarch64").exists(), "N-1 kept");
        assert!(
            d.join("kigi-0.1.138-macos-aarch64").exists(),
            "fresh just-renamed binary must NOT be deleted"
        );
        assert!(
            !d.join("kigi-0.1.139-macos-aarch64").exists(),
            "genuinely old binary still swept"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_cleanup_old_downloads_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // kigi-latest is a symlink — must be skipped.
        let target = d.join("kigi-0.1.141-macos-aarch64");
        std::fs::write(&target, "current").unwrap();
        std::os::unix::fs::symlink(&target, d.join("kigi-latest")).unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "kigi", "0.1.141").await;

        assert!(
            d.join("kigi-latest").exists(),
            "symlink must not be deleted"
        );
        assert!(target.exists(), "current must not be deleted");
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_version_prefix_collision() {
        // Regression: version "0.1.14" must not protect "0.1.140", "0.1.141".
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        std::fs::write(d.join("kigi-0.1.14-macos-aarch64"), "current").unwrap();
        std::fs::write(d.join("kigi-0.1.140-macos-aarch64"), "old-140").unwrap();
        std::fs::write(d.join("kigi-0.1.141-macos-aarch64"), "old-141").unwrap();
        std::fs::write(d.join("kigi-0.1.13-macos-aarch64"), "old-13").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "kigi", "0.1.14").await;

        assert!(d.join("kigi-0.1.14-macos-aarch64").exists(), "current");
        assert!(
            d.join("kigi-0.1.141-macos-aarch64").exists(),
            "N-1 is 0.1.141"
        );
        assert!(!d.join("kigi-0.1.140-macos-aarch64").exists());
        assert!(!d.join("kigi-0.1.13-macos-aarch64").exists());
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_alpha_and_mixed_versions() {
        // Pre-release names parse whole, and semver ordering decides N-1.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        std::fs::write(d.join("kigi-0.1.148-macos-aarch64"), "stable-148").unwrap();
        std::fs::write(d.join("kigi-0.1.149-alpha.1-macos-aarch64"), "alpha-149").unwrap();
        std::fs::write(d.join("kigi-0.1.149-macos-aarch64"), "stable-149").unwrap();
        std::fs::write(d.join("kigi-0.1.150-macos-aarch64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "kigi", "0.1.150").await;

        assert!(d.join("kigi-0.1.150-macos-aarch64").exists(), "current");
        // Newest old is 0.1.149 stable (semver: 0.1.149 > 0.1.149-alpha.1).
        assert!(
            d.join("kigi-0.1.149-macos-aarch64").exists(),
            "N-1 is stable 0.1.149"
        );
        assert!(!d.join("kigi-0.1.149-alpha.1-macos-aarch64").exists());
        assert!(!d.join("kigi-0.1.148-macos-aarch64").exists());
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_ignores_non_versioned_and_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("kigi-latest"), "alias").unwrap();
        std::fs::write(d.join("kigi-9garbage-macos-aarch64"), "junk").unwrap();
        std::fs::write(d.join("README.md"), "readme").unwrap();
        std::fs::write(d.join("other-tool-0.1.0"), "other").unwrap();
        std::fs::write(d.join("kigi-0.1.140-macos-aarch64"), "v140").unwrap();
        std::fs::write(d.join("kigi-0.1.141-macos-aarch64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "kigi", "0.1.141").await;

        assert!(d.join("kigi-latest").exists());
        assert!(
            d.join("kigi-9garbage-macos-aarch64").exists(),
            "unparseable file must be ignored, not deleted"
        );
        assert!(d.join("README.md").exists());
        assert!(d.join("other-tool-0.1.0").exists());
        assert!(d.join("kigi-0.1.140-macos-aarch64").exists(), "N-1 kept");
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_invalid_current_version_is_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("kigi-0.1.140-macos-aarch64"), "v140").unwrap();
        std::fs::write(d.join("kigi-0.1.141-macos-aarch64"), "v141").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "kigi", "not-a-version").await;
        assert!(d.join("kigi-0.1.140-macos-aarch64").exists());
        assert!(d.join("kigi-0.1.141-macos-aarch64").exists());
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_missing_dir_no_panic() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        cleanup_old_downloads(&missing, "kigi", "0.1.141").await;
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_multiplatform_in_same_dir() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // Same version, multiple platforms: both are "current" via the
        // version equality check.
        std::fs::write(d.join("kigi-0.1.141-macos-aarch64"), "mac").unwrap();
        std::fs::write(d.join("kigi-0.1.141-linux-x86_64"), "linux").unwrap();
        std::fs::write(d.join("kigi-0.1.140-macos-aarch64"), "old-mac").unwrap();
        std::fs::write(d.join("kigi-0.1.139-macos-aarch64"), "older-mac").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "kigi", "0.1.141").await;

        assert!(d.join("kigi-0.1.141-macos-aarch64").exists());
        assert!(d.join("kigi-0.1.141-linux-x86_64").exists());
        assert!(d.join("kigi-0.1.140-macos-aarch64").exists());
        assert!(!d.join("kigi-0.1.139-macos-aarch64").exists());
    }

    // reinstall_hint / manual_install_cmd

    #[test]
    fn test_reinstall_hint_points_at_repo_install_script() {
        let hint = reinstall_hint("internal");
        if cfg!(windows) {
            assert!(hint.contains("irm"), "should suggest irm install: {hint}");
            assert!(
                hint.contains("ZacharyZhang-NY/Kigi-CLI/main/install.ps1"),
                "should reference the repo's install.ps1: {hint}"
            );
        } else {
            assert!(hint.contains("curl"), "should suggest curl install: {hint}");
            assert!(
                hint.contains("ZacharyZhang-NY/Kigi-CLI/main/install.sh"),
                "should reference the repo's install.sh: {hint}"
            );
        }
        // Unknown installers fall back to the same hint.
        assert_eq!(reinstall_hint("homebrew"), hint);
        assert_eq!(reinstall_hint(""), hint);
    }

    // Asset naming: targets → release asset names

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn test_detect_platform_matches_compile_time_cfg() {
        let (os, arch) = detect_platform().unwrap();
        if cfg!(target_os = "macos") {
            assert_eq!(os, "macos");
        }
        if cfg!(target_os = "linux") {
            assert_eq!(os, "linux");
        }
        if cfg!(target_os = "windows") {
            assert_eq!(os, "windows");
        }
        if cfg!(target_arch = "x86_64") {
            assert_eq!(arch, "x86_64");
        }
        if cfg!(target_arch = "aarch64") {
            assert_eq!(arch, "aarch64");
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn test_release_asset_name_matches_release_workflow_naming() {
        // Must stay in lockstep with .github/workflows/release.yml, which
        // publishes kigi-<version>-<target-triple>.{tar.gz|zip}.
        let triple = target_triple().unwrap();
        assert!(
            [
                "aarch64-apple-darwin",
                "x86_64-apple-darwin",
                "aarch64-unknown-linux-gnu",
                "x86_64-unknown-linux-gnu",
                "x86_64-pc-windows-msvc",
            ]
            .contains(&triple),
            "triple {triple} is not one of the five released targets"
        );

        let name = release_asset_name("0.1.0").unwrap();
        if cfg!(windows) {
            assert_eq!(name, format!("kigi-0.1.0-{triple}.zip"));
        } else {
            assert_eq!(name, format!("kigi-0.1.0-{triple}.tar.gz"));
        }
    }

    // SHA256SUMS parsing

    #[test]
    fn test_expected_sha256_for_parses_sha256sum_format() {
        let sums = "\
0000000000000000000000000000000000000000000000000000000000000001  kigi-0.1.0-aarch64-apple-darwin.tar.gz
0000000000000000000000000000000000000000000000000000000000000002  kigi-0.1.0-x86_64-unknown-linux-gnu.tar.gz
0000000000000000000000000000000000000000000000000000000000000003 *kigi-0.1.0-x86_64-pc-windows-msvc.zip
";
        assert_eq!(
            expected_sha256_for(sums, "kigi-0.1.0-aarch64-apple-darwin.tar.gz").unwrap(),
            "0000000000000000000000000000000000000000000000000000000000000001"
        );
        assert_eq!(
            expected_sha256_for(sums, "kigi-0.1.0-x86_64-unknown-linux-gnu.tar.gz").unwrap(),
            "0000000000000000000000000000000000000000000000000000000000000002"
        );
        // `*name` binary-mode marker is accepted.
        assert_eq!(
            expected_sha256_for(sums, "kigi-0.1.0-x86_64-pc-windows-msvc.zip").unwrap(),
            "0000000000000000000000000000000000000000000000000000000000000003"
        );
        // Uppercase hashes normalize to lowercase.
        let upper = "ABCDEF0000000000000000000000000000000000000000000000000000000000  a.tar.gz";
        assert_eq!(
            expected_sha256_for(upper, "a.tar.gz").unwrap(),
            "abcdef0000000000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn test_expected_sha256_for_missing_or_malformed_entries_error() {
        let sums =
            "0000000000000000000000000000000000000000000000000000000000000001  present.tar.gz\n";
        let err = expected_sha256_for(sums, "absent.tar.gz").unwrap_err();
        assert!(format!("{err}").contains("no entry"), "err: {err}");

        // Truncated hash is malformed, not silently accepted.
        let bad = "deadbeef  present.tar.gz\n";
        let err = expected_sha256_for(bad, "present.tar.gz").unwrap_err();
        assert!(format!("{err}").contains("malformed"), "err: {err}");

        // Non-hex hash of the right length is malformed too.
        let nonhex = format!("{}  present.tar.gz\n", "g".repeat(64));
        let err = expected_sha256_for(&nonhex, "present.tar.gz").unwrap_err();
        assert!(format!("{err}").contains("malformed"), "err: {err}");

        // Empty manifest.
        let err = expected_sha256_for("", "present.tar.gz").unwrap_err();
        assert!(format!("{err}").contains("no entry"), "err: {err}");
    }

    #[tokio::test]
    async fn test_sha256_hex_of_file_matches_known_vector() {
        // SHA-256("abc") is a NIST test vector.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abc.txt");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_hex_of_file(&p).await.unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    // Archive extraction (Unix: tar.gz)

    #[cfg(not(windows))]
    fn make_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_extract_kigi_binary_finds_kigi_entry() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("kigi-0.1.0-test.tar.gz");
        std::fs::write(
            &archive,
            make_tar_gz(&[
                ("LICENSE", b"license text"),
                ("kigi", b"#!/bin/sh\nexit 0\n"),
            ]),
        )
        .unwrap();

        let out = dir.path().join("kigi-extracted");
        extract_kigi_binary(&archive, &out).await.unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"#!/bin/sh\nexit 0\n");
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_extract_kigi_binary_missing_entry_errors() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("kigi-0.1.0-test.tar.gz");
        std::fs::write(&archive, make_tar_gz(&[("LICENSE", b"license only")])).unwrap();

        let out = dir.path().join("kigi-extracted");
        let err = extract_kigi_binary(&archive, &out).await.unwrap_err();
        assert!(format!("{err}").contains("no 'kigi' binary"), "err: {err}");
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_extract_kigi_binary_garbage_archive_errors() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("kigi-0.1.0-test.tar.gz");
        std::fs::write(&archive, b"this is not a gzip stream").unwrap();

        let out = dir.path().join("kigi-extracted");
        assert!(extract_kigi_binary(&archive, &out).await.is_err());
    }

    // UpdateStatus serialization (camelCase contract for --json clients)

    fn make_status() -> UpdateStatus {
        UpdateStatus {
            current_version: "0.1.150".to_string(),
            latest_version: Some("0.1.151".to_string()),
            update_available: true,
            installer: Some("internal".to_string()),
            channel: "stable".to_string(),
            auto_update: Some(true),
            error: None,
        }
    }

    #[test]
    fn test_update_status_serializes_camel_case_keys() {
        let s = make_status();
        let v = serde_json::to_value(&s).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("currentVersion"));
        assert!(obj.contains_key("latestVersion"));
        assert!(obj.contains_key("updateAvailable"));
        assert!(obj.contains_key("installer"));
        assert!(obj.contains_key("channel"));
        assert!(obj.contains_key("autoUpdate"));
        assert!(obj.contains_key("error"));
        // Snake-case names must NOT leak.
        assert!(!obj.contains_key("current_version"));
        assert!(!obj.contains_key("latest_version"));
        assert!(!obj.contains_key("update_available"));
        assert!(!obj.contains_key("auto_update"));
    }

    #[test]
    fn test_update_status_field_values_round_trip_through_json() {
        let s = make_status();
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["currentVersion"], "0.1.150");
        assert_eq!(v["latestVersion"], "0.1.151");
        assert_eq!(v["updateAvailable"], true);
        assert_eq!(v["installer"], "internal");
        assert_eq!(v["channel"], "stable");
        assert_eq!(v["autoUpdate"], true);
        assert!(v["error"].is_null());
    }

    #[test]
    fn test_update_status_optional_none_serializes_to_null() {
        let s = UpdateStatus {
            current_version: "0.1.150".to_string(),
            latest_version: None,
            update_available: false,
            installer: None,
            channel: "stable".to_string(),
            auto_update: None,
            error: None,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert!(v["latestVersion"].is_null());
        assert!(v["installer"].is_null());
        assert!(v["autoUpdate"].is_null());
        assert!(v["error"].is_null());
        assert_eq!(v["updateAvailable"], false);
    }

    #[test]
    fn test_update_status_with_error_field_serialized() {
        let s = UpdateStatus {
            current_version: "0.1.150".to_string(),
            latest_version: None,
            update_available: false,
            installer: Some("internal".to_string()),
            channel: "stable".to_string(),
            auto_update: Some(true),
            error: Some("GitHub API returned HTTP 403".to_string()),
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["error"], "GitHub API returned HTTP 403");
    }

    #[test]
    fn test_update_status_json_is_valid_single_object() {
        // Whatever we add to UpdateStatus in the future, the serialization
        // must remain a single JSON object (not an array, primitive, etc.).
        let s = make_status();
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.starts_with('{'), "must be a JSON object: {json}");
        assert!(json.ends_with('}'), "must be a JSON object: {json}");
        // Single line: no embedded newlines (the wire format is one line).
        assert!(!json.contains('\n'), "must be single line: {json}");
    }

    // print_update_status — both code paths must not panic or error.

    #[test]
    fn test_print_update_status_all_shapes_return_ok() {
        print_update_status(&make_status(), true).unwrap();
        print_update_status(&make_status(), false).unwrap();
        print_update_status(
            &UpdateStatus {
                current_version: "0.1.150".to_string(),
                latest_version: None,
                update_available: false,
                installer: None,
                channel: "stable".to_string(),
                auto_update: None,
                error: None,
            },
            false,
        )
        .unwrap();
        print_update_status(
            &UpdateStatus {
                current_version: "0.1.150".to_string(),
                latest_version: Some("0.1.150".to_string()),
                update_available: false,
                installer: Some("internal".to_string()),
                channel: "stable".to_string(),
                auto_update: Some(true),
                error: Some("network down".to_string()),
            },
            false,
        )
        .unwrap();
    }

    // UpdateRunMode

    #[test]
    fn test_update_run_mode_is_copy_clone_debug() {
        // The ergonomic Copy/Clone/Debug derives must not regress: we pass
        // `run_mode` by value through several layers.
        let m1 = UpdateRunMode::Blocking;
        let m2 = m1;
        // Copy again, m1 not moved
        let m3 = m1;
        assert!(matches!(m1, UpdateRunMode::Blocking));
        assert!(matches!(m2, UpdateRunMode::Blocking));
        assert!(matches!(m3, UpdateRunMode::Blocking));
        // Debug exists.
        let _ = format!("{:?}", UpdateRunMode::NonBlocking);
    }

    // Constants — lock them in so silent renames are caught.

    #[test]
    fn test_user_facing_constants_are_stable() {
        assert_eq!(PROMPT_UPDATE_NOW, "Update now? [Y/n/d]");
        assert_eq!(
            MSG_AUTO_UPDATE_BACKGROUND,
            "Auto-update running in background."
        );
        assert_eq!(
            MSG_RUN_UPDATE_MANUAL,
            "Run `kigi update` to get the latest version."
        );
        assert_eq!(SHA256SUMS_ASSET, "SHA256SUMS");
        if cfg!(windows) {
            assert_eq!(ARCHIVE_EXT, "zip");
        } else {
            assert_eq!(ARCHIVE_EXT, "tar.gz");
        }
    }

    // env_installer — env-var based, must run serially.
    //
    // Resolution order (matches function body):
    //   1. KIGI_INSTALLER (internal; anything else → None)
    //   2. KIGI_MANAGED_BY_INTERNAL → internal
    //   3. None

    /// Snapshot every installer-related env var so the test can clear them
    /// at start and restore them at end.
    struct InstallerEnvGuard {
        prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl InstallerEnvGuard {
        fn isolate() -> Self {
            const VARS: &[&str] = &["KIGI_INSTALLER", "KIGI_MANAGED_BY_INTERNAL"];
            let prev: Vec<_> = VARS.iter().map(|k| (*k, std::env::var_os(k))).collect();
            unsafe {
                for k in VARS {
                    std::env::remove_var(k);
                }
            }
            Self { prev }
        }
    }

    impl Drop for InstallerEnvGuard {
        fn drop(&mut self) {
            unsafe {
                for (k, v) in &self.prev {
                    match v {
                        Some(val) => std::env::set_var(k, val),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_no_vars_returns_none() {
        let _g = InstallerEnvGuard::isolate();
        assert_eq!(env_installer(), None);
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_explicit_internal() {
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("KIGI_INSTALLER", "internal") };
        assert_eq!(env_installer(), Some("internal"));
        // Case-insensitive.
        unsafe { std::env::set_var("KIGI_INSTALLER", "INTERNAL") };
        assert_eq!(env_installer(), Some("internal"));
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_unknown_or_empty_returns_none() {
        // CRITICAL: when the explicit env var is set to something we don't
        // recognize, we early-return None and do NOT fall through to the
        // other env vars.
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("KIGI_INSTALLER", "npm") };
        unsafe { std::env::set_var("KIGI_MANAGED_BY_INTERNAL", "1") };
        assert_eq!(
            env_installer(),
            None,
            "explicit unknown KIGI_INSTALLER must early-return None, not fall through"
        );
        unsafe { std::env::set_var("KIGI_INSTALLER", "") };
        assert_eq!(env_installer(), None);
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_managed_by_internal() {
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("KIGI_MANAGED_BY_INTERNAL", "1") };
        assert_eq!(env_installer(), Some("internal"));
        // The check is `is_some` — any value (including empty) wins.
        unsafe { std::env::set_var("KIGI_MANAGED_BY_INTERNAL", "") };
        assert_eq!(env_installer(), Some("internal"));
    }

    // windows_replace_exe — runs only on Windows CI

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_creates_and_overwrites_dest() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new-binary.exe");
        std::fs::write(&src, "new content").unwrap();
        let dest = dir.path().join("kigi.exe");

        windows_replace_exe(&src, &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new content");

        std::fs::write(&src, "newer content").unwrap();
        windows_replace_exe(&src, &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"newer content");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_cleans_stale_old_backup() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new.exe");
        std::fs::write(&src, "new").unwrap();
        let dest = dir.path().join("kigi.exe");
        std::fs::write(&dest, "current").unwrap();
        let old = dir.path().join("kigi.exe.old");
        std::fs::write(&old, "stale-from-prior-update").unwrap();

        windows_replace_exe(&src, &dest).await.unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        assert!(!old.exists(), "stale .old must be removed");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_no_filename_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.exe");
        std::fs::write(&src, "data").unwrap();

        let bad_dest = dir.path().join("..");
        let err = windows_replace_exe(&src, &bad_dest).await.unwrap_err();
        assert!(format!("{err:#}").contains("no filename"), "error: {err:#}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_locked_file_renames_aside() {
        // Simulate a running .exe: blocks writes but allows rename.
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x00000001;
        const FILE_SHARE_DELETE: u32 = 0x00000004;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new.exe");
        std::fs::write(&src, "updated binary").unwrap();
        let dest = dir.path().join("kigi.exe");
        std::fs::write(&dest, "running binary").unwrap();

        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(&dest)
            .unwrap();

        windows_replace_exe(&src, &dest).await.unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "updated binary");

        let old = dir.path().join("kigi.exe.old");
        assert!(old.exists(), ".old must exist after rename fallback");
        drop(_lock);
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "running binary");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_rollback_on_copy_failure() {
        // No stale .old: the aside IS kigi.exe.old, so this pins the
        // non-diverted rollback branch (rename .old back onto dest).
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x00000001;
        const FILE_SHARE_DELETE: u32 = 0x00000004;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new.exe");
        std::fs::write(&src, "updated binary").unwrap();
        let dest = dir.path().join("kigi.exe");
        std::fs::write(&dest, "original").unwrap();

        // Dest locked like a running exe: blocks writes but allows rename.
        let _dest_lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(&dest)
            .unwrap();
        // Exclusive src lock: both copies fail with a sharing violation, so
        // the rename runs and the second copy triggers the rollback.
        let _src_lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&src)
            .unwrap();

        let result = windows_replace_exe(&src, &dest).await;
        drop(_src_lock);
        drop(_dest_lock);

        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "original",
            "rollback must restore the original binary"
        );
        let old = dir.path().join("kigi.exe.old");
        assert!(!old.exists(), "rollback must consume the .old aside");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_locked_stale_old_does_not_block_update() {
        // A leftover .old can still be a running image (the session live
        // during the previous update): undeletable, so the rename must
        // divert to a unique aside instead of failing on the locked name.
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x00000001;
        const FILE_SHARE_DELETE: u32 = 0x00000004;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new.exe");
        std::fs::write(&src, "updated binary").unwrap();
        let dest = dir.path().join("kigi.exe");
        std::fs::write(&dest, "running binary").unwrap();
        let old = dir.path().join("kigi.exe.old");
        std::fs::write(&old, "previous binary").unwrap();

        // No FILE_SHARE_DELETE: .old cannot be deleted or rename-replaced.
        let _old_lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&old)
            .unwrap();
        // Dest locked like a running exe: blocks writes but allows rename.
        let _dest_lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(&dest)
            .unwrap();

        windows_replace_exe(&src, &dest).await.unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "updated binary");
        assert_eq!(
            std::fs::read_to_string(&old).unwrap(),
            "previous binary",
            "locked .old must be left in place"
        );
        let asides: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("kigi.exe.old.") && n.ends_with(".old"))
            })
            .collect();
        assert_eq!(
            asides.len(),
            1,
            "dest must be renamed to a unique aside: {asides:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&asides[0]).unwrap(),
            "running binary"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_sweeps_accumulated_asides() {
        // Asides pile up while superseded sessions keep running; a later
        // update must collect the no-longer-locked ones — but never another
        // executable's leftovers.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new.exe");
        std::fs::write(&src, "new").unwrap();
        let dest = dir.path().join("kigi.exe");
        std::fs::write(&dest, "current").unwrap();
        let old = dir.path().join("kigi.exe.old");
        std::fs::write(&old, "stale").unwrap();
        let aside_a = dir.path().join("kigi.exe.old.1234-0.old");
        let aside_b = dir.path().join("kigi.exe.old.1234-1.old");
        std::fs::write(&aside_a, "aside-a").unwrap();
        std::fs::write(&aside_b, "aside-b").unwrap();
        let other_old = dir.path().join("other.exe.old");
        std::fs::write(&other_old, "other-old").unwrap();

        windows_replace_exe(&src, &dest).await.unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        assert!(!old.exists(), "legacy .old must be swept");
        assert!(!aside_a.exists(), "aside must be swept");
        assert!(!aside_b.exists(), "aside must be swept");
        assert!(
            other_old.exists(),
            "other executables' leftovers must be untouched"
        );
    }
}
