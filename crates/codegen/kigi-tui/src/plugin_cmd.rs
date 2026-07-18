//! `kigi plugin` CLI subcommand — manage installed plugins.
//!
//! Follows the `memory_cmd.rs` / `sessions_cmd.rs` / `worktree_cmd` pattern:
//! clap args and handler logic co-located in a dedicated module. The pager's
//! `main.rs` dispatches here with a one-liner.
//!
//! Business logic lives in `kigi_shell::plugin` (shared orchestration) and
//! lower crates (`kigi-agent`). This module is a thin CLI wrapper: parse args,
//! call ops, and format output.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Subcommand;
use serde::Serialize;

use kigi_agent::plugins::install_registry::{InstallKind, InstallRegistry};
use kigi_agent::plugins::manifest::{ManifestLoadResult, PluginManifest, load_manifest};
use kigi_shell::plugin::{self, RepoUpdateOutcome, UninstallError};

// ── JSON output types ───────────────────────────────────────────────

/// Typed entry for `kigi plugin list --json`. The `status` field is a stable
/// discriminator for machine consumers.
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum PluginEntry {
    Installed {
        name: String,
        repo_key: String,
        version: Option<String>,
        path: PathBuf,
        source: String,
    },
}

// ── CLI arg definitions ─────────────────────────────────────────────

#[derive(Debug, clap::Args, Clone)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub command: PluginCommand,
}

#[derive(Debug, Subcommand, Clone)]
pub enum PluginCommand {
    /// List installed plugins
    List {
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Install a plugin from a git URL or local path
    Install {
        /// Git URL, GitHub shorthand (user/repo), or local path.
        /// Supports @ref suffix (e.g. user/repo@v1.0) and #subdir.
        source: String,
        /// Trust the plugin immediately (skip confirmation prompt).
        #[arg(long)]
        trust: bool,
    },
    /// Uninstall an installed plugin by name
    #[command(visible_alias = "rm", visible_alias = "remove")]
    Uninstall {
        /// Plugin name (as shown by `kigi plugin list`).
        name: String,
        /// Skip confirmation for multi-plugin repos.
        #[arg(long)]
        confirm: bool,
        /// Preserve the plugin's persistent data directory.
        #[arg(long)]
        keep_data: bool,
    },
    /// Update installed plugin(s)
    Update {
        /// Plugin name to update. Omit to update all.
        name: Option<String>,
    },
    /// Enable a disabled plugin
    Enable {
        /// Plugin name to enable.
        name: String,
    },
    /// Disable a plugin without uninstalling it
    Disable {
        /// Plugin name to disable.
        name: String,
    },
    /// Show a plugin's component inventory
    Details {
        /// Plugin name.
        name: String,
    },
    /// Validate a plugin manifest
    Validate {
        /// Path to plugin directory (default: current directory).
        #[arg(default_value = ".")]
        path: String,
    },
    /// Create a release git tag from the plugin's manifest version
    Tag {
        /// Path to plugin directory (default: current directory).
        #[arg(default_value = ".")]
        path: String,
        /// Push the tag to the remote after creating it.
        #[arg(long)]
        push: bool,
        /// Create the tag even if the working tree is dirty or tag exists.
        #[arg(long, short = 'f')]
        force: bool,
        /// Print what would be tagged without creating the tag.
        #[arg(long)]
        dry_run: bool,
    },
}

// ── Helpers ─────────────────────────────────────────────────────────

fn kind_label(kind: &InstallKind) -> String {
    match kind {
        InstallKind::Git { url, .. } => format!("git: {url}"),
        InstallKind::Local { source_path, .. } => format!("local: {}", source_path.display()),
    }
}

fn print_component_summary(manifest: &PluginManifest, root: &Path) {
    let skills = manifest.skill_dirs(root);
    let commands = manifest.command_dirs(root);
    let agents = manifest.agent_dirs(root);
    let has_hooks = manifest.hooks_path(root).is_some() || manifest.inline_hooks().is_some();
    let has_mcp =
        manifest.mcp_config_path(root).is_some() || manifest.inline_mcp_servers().is_some();
    let has_lsp =
        manifest.lsp_config_path(root).is_some() || manifest.inline_lsp_servers().is_some();
    println!(
        "  components: {} skill dir(s), {} command dir(s), {} agent dir(s){}{}{}",
        skills.len(),
        commands.len(),
        agents.len(),
        if has_hooks { ", hooks" } else { "" },
        if has_mcp { ", MCP servers" } else { "" },
        if has_lsp { ", LSP servers" } else { "" },
    );
}

fn abbreviated_commit(c: Option<&str>) -> &str {
    c.map(|s| &s[..7.min(s.len())]).unwrap_or("?")
}

fn trust_prompt(subject: &str, source_arg: &str) -> String {
    format!(
        "Installing {subject} requires confirmation.\n\
         Plugins can run hooks, MCP servers, and skills on your machine, so installation needs explicit trust.\n\
         \n\
         To proceed, re-run with --trust:\n  kigi plugin install {source_arg} --trust"
    )
}

// ── Top-level dispatch ──────────────────────────────────────────────

pub async fn run(args: PluginArgs) -> Result<()> {
    match args.command {
        PluginCommand::List { json } => cmd_list(json),
        PluginCommand::Install { source, trust } => cmd_install(&source, trust),
        PluginCommand::Uninstall {
            name,
            confirm,
            keep_data,
        } => cmd_uninstall(&name, confirm, keep_data),
        PluginCommand::Update { name } => cmd_update(name.as_deref()),
        PluginCommand::Enable { name } => cmd_enable(&name),
        PluginCommand::Disable { name } => cmd_disable(&name),
        PluginCommand::Details { name } => cmd_details(&name),
        PluginCommand::Validate { path } => cmd_validate(&path),
        PluginCommand::Tag {
            path,
            push,
            force,
            dry_run,
        } => cmd_tag(&path, push, force, dry_run),
    }
}

// ── Plugin subcommands ──────────────────────────────────────────────

fn cmd_list(json: bool) -> Result<()> {
    let registry = InstallRegistry::load();
    let repos = registry.list();

    if json {
        let entries = installed_plugins(&repos);
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else if repos.is_empty() {
        println!("No plugins installed. Run `kigi plugin install --help` to get started.");
    } else {
        for (repo_key, repo) in &repos {
            let names: Vec<&str> = repo.plugins.keys().map(|s| s.as_str()).collect();
            println!(
                "  {repo_key}: {} [{}]",
                names.join(", "),
                kind_label(&repo.kind)
            );
        }
    }
    Ok(())
}

fn installed_plugins(
    repos: &[(&str, &kigi_agent::plugins::install_registry::InstalledRepo)],
) -> Vec<PluginEntry> {
    repos
        .iter()
        .flat_map(|(repo_key, repo)| {
            let source = match &repo.kind {
                InstallKind::Git { url, .. } => url.clone(),
                InstallKind::Local { source_path, .. } => source_path.display().to_string(),
            };
            repo.plugins
                .iter()
                .map(move |(name, plugin)| PluginEntry::Installed {
                    name: name.clone(),
                    repo_key: repo_key.to_string(),
                    version: plugin.version.clone(),
                    path: repo.path.clone(),
                    source: source.clone(),
                })
        })
        .collect()
}

fn cmd_install(source: &str, trust: bool) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_default();

    if !trust {
        use kigi_agent::plugins::git_install::{self, InstallSource};
        let subject = match git_install::parse_install_source(source, &cwd) {
            InstallSource::Git { url, .. } => format!("from git repo {url}"),
            InstallSource::Local { path, .. } => format!("from directory {}", path.display()),
        };
        eprintln!("{}", trust_prompt(&subject, source));
        std::process::exit(1);
    }

    match plugin::install_plugin(source, &cwd) {
        Ok(outcome) => {
            for w in &outcome.warnings {
                tracing::warn!("{w}");
            }
            println!(
                "Installed {} plugin(s) from {source}: {}",
                outcome.plugin_names.len(),
                outcome.plugin_names.join(", "),
            );
            Ok(())
        }
        Err(e) => bail!("{e}"),
    }
}

fn cmd_uninstall(name: &str, confirm: bool, keep_data: bool) -> Result<()> {
    match plugin::uninstall_plugin(name, confirm, keep_data) {
        Ok(outcome) => {
            let suffix = if keep_data { " (data preserved)" } else { "" };
            println!(
                "Uninstalled {} plugin(s): {}{suffix}",
                outcome.removed_plugins.len(),
                outcome.removed_plugins.join(", "),
            );
            Ok(())
        }
        Err(UninstallError::NeedsConfirm {
            name,
            repo_key,
            other_plugins,
            total,
        }) => bail!(
            "Plugin \"{name}\" belongs to repo \"{repo_key}\" which also contains:\n\
             {}\n\n\
             Uninstalling will remove all {total} plugin(s). To proceed:\n\
               kigi plugin uninstall {name} --confirm",
            other_plugins
                .iter()
                .map(|p| format!("  - {p}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Err(e @ UninstallError::NotFound { .. }) => bail!("{e}"),
    }
}

fn cmd_update(name: Option<&str>) -> Result<()> {
    let outcomes = plugin::update_plugins(name).map_err(|e| anyhow::anyhow!("{e}"))?;

    if outcomes.is_empty() {
        println!("No installed plugins to update.");
        return Ok(());
    }

    for o in &outcomes {
        match o {
            RepoUpdateOutcome::Updated {
                repo_key,
                old_commit,
                new_commit,
            } => {
                println!(
                    "{repo_key}: updated ({} -> {})",
                    abbreviated_commit(old_commit.as_deref()),
                    abbreviated_commit(new_commit.as_deref()),
                );
            }
            RepoUpdateOutcome::AlreadyUpToDate { repo_key } => {
                println!("{repo_key}: already up to date");
            }
            RepoUpdateOutcome::Pinned { repo_key, ref_name } => {
                println!("{repo_key}: pinned to {ref_name}, skipping");
            }
            RepoUpdateOutcome::LiveLocal { repo_key } => {
                println!("{repo_key}: local symlink, already live");
            }
            RepoUpdateOutcome::Failed { repo_key, error } => {
                eprintln!("{repo_key}: update failed: {error}");
            }
        }
    }
    Ok(())
}

fn cmd_enable(name: &str) -> Result<()> {
    let registry = InstallRegistry::load();
    if registry.find_plugin(name).is_none() {
        bail!(
            "Plugin \"{name}\" not found.\n\
               Run `kigi plugin list` to see installed plugins."
        );
    }
    if let Err(e) = kigi_shell::config::remove_disabled_plugin(name) {
        tracing::warn!("failed to remove from disabled list: {e}");
    }
    kigi_shell::config::add_enabled_plugin(name)
        .map_err(|e| anyhow::anyhow!("Failed to enable plugin: {e}"))?;
    println!("Enabled plugin: {name}");
    Ok(())
}

fn cmd_disable(name: &str) -> Result<()> {
    let registry = InstallRegistry::load();
    if registry.find_plugin(name).is_none() {
        bail!(
            "Plugin \"{name}\" not found.\n\
               Run `kigi plugin list` to see installed plugins."
        );
    }
    if let Err(e) = kigi_shell::config::remove_enabled_plugin(name) {
        tracing::warn!("failed to remove from enabled list: {e}");
    }
    kigi_shell::config::add_disabled_plugin(name)
        .map_err(|e| anyhow::anyhow!("Failed to disable plugin: {e}"))?;
    println!("Disabled plugin: {name}");
    Ok(())
}

fn cmd_details(name: &str) -> Result<()> {
    let registry = InstallRegistry::load();
    let (repo_key, repo, _) = registry.find_plugin(name).ok_or_else(|| {
        anyhow::anyhow!(
            "Plugin \"{name}\" not found.\n\
             Run `kigi plugin list` to see installed plugins."
        )
    })?;

    println!("{repo_key}");
    println!("  path: {}", repo.path.display());
    println!("  kind: {}", kind_label(&repo.kind));
    println!("  installed: {}", repo.installed_at);
    println!("  updated: {}", repo.updated_at);
    println!("  plugins ({}):", repo.plugins.len());
    for (pname, p) in &repo.plugins {
        let ver = p
            .version
            .as_deref()
            .map(|v| format!(" v{v}"))
            .unwrap_or_default();
        let sub = p
            .subdir
            .as_deref()
            .map(|s| format!(" (subdir: {s})"))
            .unwrap_or_default();
        println!("    {pname}{ver}{sub}");
    }

    if let Ok(ManifestLoadResult::Found(manifest)) = load_manifest(&repo.path) {
        if let Some(ref desc) = manifest.description {
            println!("  description: {desc}");
        }
        print_component_summary(&manifest, &repo.path);
    }
    Ok(())
}

fn cmd_validate(path: &str) -> Result<()> {
    let root = PathBuf::from(path);
    if !root.is_dir() {
        bail!("Not a directory: {path}");
    }
    match load_manifest(&root) {
        Ok(ManifestLoadResult::Found(manifest)) => {
            manifest
                .validate()
                .map_err(|e| anyhow::anyhow!("Manifest validation failed: {e}"))?;
            println!("Plugin manifest is valid.");
            println!("  name: {}", manifest.name);
            if let Some(ref v) = manifest.version {
                println!("  version: {v}");
            }
            if let Some(ref d) = manifest.description {
                println!("  description: {d}");
            }
            print_component_summary(&manifest, &root);
            Ok(())
        }
        Ok(ManifestLoadResult::NotFound) => {
            println!(
                "No plugin.json found. Kigi discovers skills, agents, and hooks \
                 automatically from standard directories. A manifest is only needed \
                 for custom paths or metadata."
            );
            Ok(())
        }
        Err(e) => bail!("Failed to load manifest: {e}"),
    }
}

fn cmd_tag(path: &str, push: bool, force: bool, dry_run: bool) -> Result<()> {
    let root = PathBuf::from(path);
    if !root.is_dir() {
        bail!("Not a directory: {path}");
    }
    let version = match load_manifest(&root) {
        Ok(ManifestLoadResult::Found(m)) => m.version.ok_or_else(|| {
            anyhow::anyhow!(
                "No `version` field in plugin.json. Set a version to use `kigi plugin tag`."
            )
        })?,
        Ok(ManifestLoadResult::NotFound) => bail!("No plugin.json found in {path}."),
        Err(e) => bail!("Failed to load manifest: {e}"),
    };

    let tag = format!(
        "v{}",
        version
            .strip_prefix('v')
            .or_else(|| version.strip_prefix('V'))
            .unwrap_or(&version)
    );

    if !force {
        let out = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&root)
            .output()?;
        if !out.stdout.is_empty() {
            bail!("Working tree is dirty. Commit changes first, or use --force.");
        }
    }

    if dry_run {
        println!("Would create tag: {tag}");
        if push {
            println!("Would push tag to remote.");
        }
        return Ok(());
    }

    let mut cmd = std::process::Command::new("git");
    cmd.args(["tag", &tag]);
    if force {
        cmd.arg("--force");
    }
    let out = cmd.current_dir(&root).output()?;
    if !out.status.success() {
        bail!(
            "Failed to create tag: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    println!("Created tag: {tag}");

    if push {
        let mut push_cmd = std::process::Command::new("git");
        push_cmd.args(["push", "origin", &tag]);
        if force {
            push_cmd.arg("--force");
        }
        let out = push_cmd.current_dir(&root).output()?;
        if !out.status.success() {
            bail!(
                "Failed to push tag: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        println!("Pushed tag {tag} to origin.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_prompt_git_and_local_subjects() {
        let git = trust_prompt("from git repo https://github.com/u/r", "u/r");
        assert!(
            git.starts_with(
                "Installing from git repo https://github.com/u/r requires confirmation."
            ),
            "{git}"
        );
        assert!(git.ends_with("  kigi plugin install u/r --trust"), "{git}");
        let local = trust_prompt("from directory /tmp/p", "./p");
        assert!(
            local.starts_with("Installing from directory /tmp/p requires confirmation."),
            "{local}"
        );
    }
}
