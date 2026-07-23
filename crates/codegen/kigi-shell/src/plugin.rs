//! Shared plugin lifecycle operations (output-agnostic).
//!
//! Called by the CLI (`plugin_cmd.rs`). The in-session slash commands
//! (`acp_session.rs`) currently inline similar logic and should migrate here.
//!
//! Callers own output formatting.

use std::path::{Path, PathBuf};

use kigi_agent::plugins::discovery::PluginScope;
use kigi_agent::plugins::git_install::{self, UpdateStatus};
use kigi_agent::plugins::install_registry::{
    InstallError, InstallKind, InstallRegistry, InstalledRepo,
};

fn save_registry_or_warn(registry: &InstallRegistry) {
    if let Err(e) = registry.save() {
        tracing::warn!("failed to save install registry: {e}");
    }
}

pub struct InstallOutcome {
    pub repo_key: String,
    pub plugin_names: Vec<String>,
    pub warnings: Vec<String>,
    /// Whether the source was a local path (vs git).
    pub is_local: bool,
}

/// Classify an install source as local (filesystem) vs git (remote) without
/// installing — used to label the install kind on the failure path, where no
/// [`InstallOutcome`] is available.
pub fn install_source_is_local(source: &str, cwd: &Path) -> bool {
    matches!(
        git_install::parse_install_source(source, cwd),
        git_install::InstallSource::Local { .. }
    )
}

/// Parse, clone/symlink, register, and enable a plugin.
pub fn install_plugin(source: &str, cwd: &Path) -> Result<InstallOutcome, InstallError> {
    let install_source = git_install::parse_install_source(source, cwd);
    let is_local = matches!(install_source, git_install::InstallSource::Local { .. });
    let mut registry = InstallRegistry::load();

    let result = git_install::install_from_source(&install_source, &registry)?;

    let repo = git_install::build_installed_repo(&result, &install_source);
    registry.insert(result.repo_key.clone(), repo);
    save_registry_or_warn(&registry);

    let (plugin_names, post_warnings) = crate::config::post_install_plugin(&result.repo_key);

    Ok(InstallOutcome {
        repo_key: result.repo_key,
        plugin_names,
        warnings: post_warnings,
        is_local,
    })
}

pub struct UninstallOutcome {
    pub repo_key: String,
    pub removed_plugins: Vec<String>,
}

pub enum UninstallError {
    NotFound {
        name: String,
    },
    NeedsConfirm {
        name: String,
        repo_key: String,
        other_plugins: Vec<String>,
        total: usize,
    },
}

impl std::fmt::Display for UninstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { name } => {
                write!(
                    f,
                    "Plugin \"{name}\" not found.\n\
                     Run `kigi plugin list` to see installed plugins."
                )
            }
            Self::NeedsConfirm {
                name,
                repo_key,
                other_plugins,
                total,
            } => {
                writeln!(
                    f,
                    "Plugin \"{name}\" belongs to repo \"{repo_key}\" which also contains:"
                )?;
                for p in other_plugins {
                    writeln!(f, "  - {p}")?;
                }
                writeln!(f)?;
                write!(f, "Uninstalling will remove all {total} plugin(s).")
            }
        }
    }
}

/// Find, remove, clean up, and deregister a plugin.
/// When `keep_data` is true, `~/.kigi/plugin-data/<id>/` is preserved.
pub fn uninstall_plugin(
    name: &str,
    confirm: bool,
    keep_data: bool,
) -> Result<UninstallOutcome, UninstallError> {
    let mut registry = InstallRegistry::load();
    let (repo_key, repo) = match registry.find_plugin(name) {
        Some((k, r, _)) => (k.to_string(), r.clone()),
        None => {
            return Err(UninstallError::NotFound {
                name: name.to_string(),
            });
        }
    };

    let removed_plugins: Vec<String> = repo.plugins.keys().cloned().collect();

    if removed_plugins.len() > 1 && !confirm {
        let others: Vec<_> = removed_plugins
            .iter()
            .filter(|p| p.as_str() != name)
            .cloned()
            .collect();
        return Err(UninstallError::NeedsConfirm {
            name: name.to_string(),
            repo_key,
            other_plugins: others,
            total: removed_plugins.len(),
        });
    }

    if let Err(e) = git_install::remove_repo_path(&repo.path) {
        tracing::warn!("failed to remove repo path: {e}");
    }

    if !keep_data {
        // Plugins under $HOME are user-scope; everything else is config-path scope.
        let scope = match dirs::home_dir() {
            Some(home) if repo.path.starts_with(&home) => PluginScope::User,
            _ => PluginScope::ConfigPath,
        };
        git_install::cleanup_plugin_data(&repo, scope);
    }

    registry.remove(&repo_key);
    save_registry_or_warn(&registry);

    Ok(UninstallOutcome {
        repo_key,
        removed_plugins,
    })
}

pub enum RepoUpdateOutcome {
    Updated {
        repo_key: String,
        old_commit: Option<String>,
        new_commit: Option<String>,
    },
    AlreadyUpToDate {
        repo_key: String,
    },
    Pinned {
        repo_key: String,
        ref_name: String,
    },
    LiveLocal {
        repo_key: String,
    },
    Failed {
        repo_key: String,
        error: String,
    },
}

pub enum UpdateError {
    NotFound { name: String },
}

pub enum PluginUpdateSelector {
    PluginName(String),
    RepoKey(String),
}

pub fn repo_update_requires_reload(outcome: &RepoUpdateOutcome) -> bool {
    matches!(outcome, RepoUpdateOutcome::Updated { .. })
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { name } => {
                write!(
                    f,
                    "Plugin \"{name}\" not found.\n\
                     Run `kigi plugin list` to see installed plugins."
                )
            }
        }
    }
}

fn apply_update_to_registry(
    registry: &mut InstallRegistry,
    repo_key: &str,
    result: &git_install::UpdateResult,
) {
    let Some(entry) = registry.get_repo_mut(repo_key) else {
        return;
    };
    if let InstallKind::Git { ref mut commit, .. } = entry.kind {
        *commit = result.new_commit.clone().unwrap_or_default();
    }
    entry.updated_at = chrono::Utc::now().to_rfc3339();
    entry.plugins = git_install::repo_plugin_map(&result.plugins);
}

/// Update one or all installed plugins. Saves the registry once at the end.
pub fn update_plugins(name: Option<&str>) -> Result<Vec<RepoUpdateOutcome>, UpdateError> {
    update_plugins_by_selector(name.map(|name| PluginUpdateSelector::PluginName(name.to_string())))
}

pub fn update_plugins_by_selector(
    selector: Option<PluginUpdateSelector>,
) -> Result<Vec<RepoUpdateOutcome>, UpdateError> {
    let mut registry = InstallRegistry::load();
    let repos_to_update: Vec<(String, InstalledRepo)> = match selector {
        Some(PluginUpdateSelector::PluginName(plugin_name)) => {
            match registry.find_plugin(&plugin_name) {
                Some((key, repo, _)) => vec![(key.to_string(), repo.clone())],
                None => {
                    return Err(UpdateError::NotFound {
                        name: plugin_name.to_string(),
                    });
                }
            }
        }
        Some(PluginUpdateSelector::RepoKey(repo_key)) => match registry.get_repo(&repo_key) {
            Some(repo) => vec![(repo_key.to_string(), repo.clone())],
            None => {
                return Err(UpdateError::NotFound {
                    name: repo_key.to_string(),
                });
            }
        },
        None => registry
            .list()
            .into_iter()
            .map(|(k, r)| (k.to_string(), r.clone()))
            .collect(),
    };

    let mut outcomes = Vec::with_capacity(repos_to_update.len());

    for (repo_key, repo) in &repos_to_update {
        let outcome = match git_install::update_repo(repo_key, repo) {
            Ok(UpdateStatus::Updated(result)) if result.changed => {
                apply_update_to_registry(&mut registry, repo_key, &result);
                RepoUpdateOutcome::Updated {
                    repo_key: repo_key.clone(),
                    old_commit: result.old_commit,
                    new_commit: result.new_commit,
                }
            }
            Ok(UpdateStatus::Updated(_)) => RepoUpdateOutcome::AlreadyUpToDate {
                repo_key: repo_key.clone(),
            },
            Ok(UpdateStatus::Pinned { ref_name }) => RepoUpdateOutcome::Pinned {
                repo_key: repo_key.clone(),
                ref_name,
            },
            Ok(UpdateStatus::LiveLocal) => RepoUpdateOutcome::LiveLocal {
                repo_key: repo_key.clone(),
            },
            Err(e) => RepoUpdateOutcome::Failed {
                repo_key: repo_key.clone(),
                error: e.to_string(),
            },
        };
        outcomes.push(outcome);
    }

    save_registry_or_warn(&registry);

    Ok(outcomes)
}

/// Expand GitHub shorthand (user/repo) to `https://github.com/user/repo.git`.
pub fn normalize_git_url(input: &str) -> String {
    if !input.contains("://") && !input.contains("git@") {
        format!("https://github.com/{}.git", input.trim_end_matches(".git"))
    } else {
        input.to_string()
    }
}

/// Derive a display name from the last path segment of a URL.
pub fn name_from_url(url: &str) -> String {
    let name = url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit('/')
        .next()
        .unwrap_or("plugin");
    if name.is_empty() {
        "plugin".to_string()
    } else {
        name.to_string()
    }
}

/// Derive a display name from the last component of a local path.
pub fn name_from_path(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "plugin".to_string())
}

/// Classify an install error into a stable category. Strings match `acp_session.rs` exactly.
pub fn classify_install_error(err: &InstallError) -> String {
    match err {
        InstallError::AlreadyInstalled { .. } => "already_installed",
        InstallError::Io { .. } => "io",
        InstallError::Json { .. } => "json",
        InstallError::PluginNotFound { .. } => "not_found",
        InstallError::ShaMismatch { .. } => "sha_mismatch",
        InstallError::InstallFailed { .. } => "install_failed",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn normalize_github_shorthand() {
        assert_eq!(
            normalize_git_url("user/repo"),
            "https://github.com/user/repo.git"
        );
        // .git suffix not doubled
        assert_eq!(
            normalize_git_url("user/repo.git"),
            "https://github.com/user/repo.git"
        );
    }

    #[test]
    fn name_from_path_uses_last_component() {
        assert_eq!(name_from_path(Path::new("/a/b/my-plugins")), "my-plugins");
        assert_eq!(name_from_path(Path::new("/a/b/my-plugins/")), "my-plugins");
        assert_eq!(name_from_path(Path::new("/")), "plugin");
    }

    #[test]
    fn name_from_url_extracts_last_segment() {
        assert_eq!(
            name_from_url("https://github.com/org/my-marketplace.git"),
            "my-marketplace"
        );
    }

    #[test]
    fn name_from_url_edge_cases() {
        // trailing slash bug fix
        assert_eq!(name_from_url("https://github.com/org/repo/"), "repo");
        // empty fallback
        assert_eq!(name_from_url(""), "plugin");
    }

    #[test]
    fn classify_error_strings_match_canonical() {
        // Must match acp_session.rs::classify_install_error exactly — prevents category drift.
        assert_eq!(
            classify_install_error(&InstallError::AlreadyInstalled { key: "k".into() }),
            "already_installed"
        );
        assert_eq!(
            classify_install_error(&InstallError::Io {
                path: "p".into(),
                source: std::io::Error::other("x")
            }),
            "io"
        );
        assert_eq!(
            classify_install_error(&InstallError::Json { detail: "x".into() }),
            "json"
        );
        assert_eq!(
            classify_install_error(&InstallError::PluginNotFound { name: "x".into() }),
            "not_found"
        );
        assert_eq!(
            classify_install_error(&InstallError::ShaMismatch {
                expected: "a".into(),
                actual: "b".into()
            }),
            "sha_mismatch"
        );
        assert_eq!(
            classify_install_error(&InstallError::InstallFailed { detail: "x".into() }),
            "install_failed"
        );
    }

    #[test]
    fn update_requires_reload_for_changed_repo_updates_only() {
        assert!(repo_update_requires_reload(&RepoUpdateOutcome::Updated {
            repo_key: "git".into(),
            old_commit: Some("a".into()),
            new_commit: Some("b".into()),
        }));
        assert!(!repo_update_requires_reload(
            &RepoUpdateOutcome::LiveLocal {
                repo_key: "local".into(),
            }
        ));
    }

    #[test]
    fn local_update_remains_noop() {
        let repo = InstalledRepo {
            kind: InstallKind::Local {
                source_path: PathBuf::from("/tmp/plugin"),
                subdir: None,
            },
            installed_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            path: PathBuf::from("/tmp/installed"),
            plugins: HashMap::new(),
        };
        let status = git_install::update_repo("local", &repo).unwrap();
        assert!(matches!(status, UpdateStatus::LiveLocal));
    }
}
