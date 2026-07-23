//! Built-in files extracted to `~/.kigi/` on startup.

const BUNDLED_FILES: &[(&str, &str)] = &[("README.md", include_str!("../README.md"))];

const HELP_SKILL_MD: &str = include_str!("../skills/help/SKILL.md");
const CREATE_SKILL_MD: &str = include_str!("../skills/create-skill/SKILL.md");
const CODE_REVIEW_SKILL_MD: &str = include_str!("../skills/code-review/SKILL.md");
/// Compiled-in SKILL.md content for `/check-work` (available to headless mode).
pub const CHECK_SKILL_MD: &str = include_str!("../skills/check-work/SKILL.md");
/// Compiled-in SKILL.md content for headless `--best-of-n` (not extracted as
/// a bundled skill).
pub const BEST_OF_N_SKILL_MD: &str = include_str!("../skills/best-of-n/SKILL.md");

/// Names of bundled skills that were renamed or removed. Their directories
/// under `~/.kigi/skills/` are deleted early in `extract_bundled_files` so an
/// old slash command (e.g. `/check` after the rename to `/check-work`) does
/// not linger after an upgrade. A name still present in `BUNDLED_SKILLS` is
/// never deleted, so a skill name can be safely re-introduced without first
/// removing its legacy entry.
const LEGACY_BUNDLED_SKILL_NAMES: &[&str] =
    &["check", "best-of-n", "docx", "pptx", "xlsx", "imagine"];

/// All bundled skill SKILL.md files. Single source of truth for both the
/// full extraction path (version bump) and the missing-file fast path
/// (same version).
///
/// When renaming a bundled skill, also add the old name to
/// `LEGACY_BUNDLED_SKILL_NAMES` so `remove_legacy_bundled_skills` cleans up
/// the old directory on the next upgrade.
const BUNDLED_SKILLS: &[(&str, &str)] = &[
    ("help", HELP_SKILL_MD),
    ("create-skill", CREATE_SKILL_MD),
    ("code-review", CODE_REVIEW_SKILL_MD),
    ("check-work", CHECK_SKILL_MD),
];

/// True when a discovered skill is the copy `extract_bundled_files` wrote to
/// `<kigi_home>/skills/<name>/SKILL.md`. Matches the exact path, not a prefix,
/// so a user-authored skill that reuses a bundled name is never labeled
/// bundled. Used by inspect, which otherwise sees extracted copies as user
/// skills.
pub(crate) fn is_extracted_bundled_skill(
    name: &str,
    path: &std::path::Path,
    kigi_home: &std::path::Path,
) -> bool {
    BUNDLED_SKILLS.iter().any(|&(n, _)| n == name)
        && path == kigi_home.join("skills").join(name).join("SKILL.md")
}

/// Resolve the content for a skill, applying any name-specific transforms.
fn resolve_skill_content(name: &str, raw: &str, kigi_home: &std::path::Path) -> String {
    match name {
        // Help skill needs path substitution so absolute paths work.
        "help" => {
            let kigi_home_str = format!("{}/", kigi_home.to_string_lossy());
            raw.replace("~/.kigi/", &kigi_home_str)
        }
        _ => raw.to_string(),
    }
}

/// Extract bundled files to `~/.kigi/` on startup.
///
/// Full extraction runs on every version bump. On same-version startups,
/// a lightweight check ensures all expected skill files exist on disk —
/// any missing files are extracted individually.
///
/// Legacy/renamed bundled skills (see `LEGACY_BUNDLED_SKILL_NAMES`) are
/// always cleaned up first so that old slash commands disappear after
/// a rename (e.g. the previous `/check` after the move to `/check-work`).
pub fn extract_bundled_files(kigi_home: &std::path::Path) {
    // Runs before the version check so renamed skills are cleaned up on
    // every startup, not only on a version bump.
    remove_legacy_bundled_skills(kigi_home);

    let version = kigi_version::VERSION;
    let marker = kigi_home.join(".metadata_version");

    if let Ok(existing) = std::fs::read_to_string(&marker)
        && existing.trim() == version
    {
        // Same version — only extract skill files that are missing on disk.
        // This handles skills added between version bumps.
        extract_missing_skills(kigi_home);
        return;
    }

    let _ = std::fs::create_dir_all(kigi_home);

    // Clean up changelog caches written by the removed changelog feature
    // (kigi <= 0.1.0 cached CDN release notes in the kigi home).
    for stale in &["CHANGELOG.json", "CHANGELOG.md"] {
        let _ = std::fs::remove_file(kigi_home.join(stale));
    }

    for &(filename, content) in BUNDLED_FILES {
        if let Err(e) = std::fs::write(kigi_home.join(filename), content) {
            tracing::debug!(error = %e, filename, "Failed to extract bundled file");
        }
    }

    for &(name, raw) in BUNDLED_SKILLS {
        let skill_dir = kigi_home.join("skills").join(name);
        let _ = std::fs::create_dir_all(&skill_dir);
        let content = resolve_skill_content(name, raw, kigi_home);
        if let Err(e) = std::fs::write(skill_dir.join("SKILL.md"), content) {
            tracing::debug!(error = %e, name, "Failed to write skill");
        }
    }

    let _ = std::fs::write(&marker, version);
    tracing::debug!(version, "Extracted bundled files");
}

/// Extract only missing skill SKILL.md files (same-version fast path).
/// Iterates `BUNDLED_SKILLS` so adding a new skill there is sufficient.
fn extract_missing_skills(kigi_home: &std::path::Path) {
    for &(name, raw) in BUNDLED_SKILLS {
        let skill_md = kigi_home.join("skills").join(name).join("SKILL.md");
        if skill_md.exists() {
            continue;
        }
        let _ = std::fs::create_dir_all(skill_md.parent().unwrap());
        let content = resolve_skill_content(name, raw, kigi_home);
        let _ = std::fs::write(&skill_md, content);
    }
}

/// Delete directories for renamed/removed bundled skills. Runs on every
/// startup; idempotent. A name still in `BUNDLED_SKILLS` is never deleted.
fn remove_legacy_bundled_skills(kigi_home: &std::path::Path) {
    remove_legacy_skills(kigi_home, LEGACY_BUNDLED_SKILL_NAMES, BUNDLED_SKILLS);
}

/// Core implementation, extracted for testability.
fn remove_legacy_skills(
    kigi_home: &std::path::Path,
    legacy_names: &[&str],
    bundled_skills: &[(&str, &str)],
) {
    for name in legacy_names {
        // Never delete a name currently shipped in `bundled_skills`, so a
        // re-introduced skill name can keep its legacy entry.
        if bundled_skills.iter().any(|(n, _)| *n == *name) {
            continue;
        }

        let dir = kigi_home.join("skills").join(name);
        if dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                tracing::debug!(error = %e, name, "Failed to remove legacy bundled skill");
            } else {
                tracing::debug!(name, "Removed legacy bundled skill directory");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_bump_re_extracts_all_files() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        extract_bundled_files(home);

        for &(filename, _) in BUNDLED_FILES {
            std::fs::write(home.join(filename), "old").unwrap();
        }
        std::fs::write(home.join("skills/help/SKILL.md"), "old").unwrap();
        for name in ["check-work", "code-review"] {
            std::fs::write(home.join(format!("skills/{name}/SKILL.md")), "old").unwrap();
        }
        std::fs::write(home.join(".metadata_version"), "0.0.0-stale").unwrap();

        // Simulate legacy skills that should be cleaned up.
        for name in ["check", "best-of-n", "docx", "pptx", "xlsx", "imagine"] {
            std::fs::create_dir_all(home.join(format!("skills/{name}"))).unwrap();
            std::fs::write(
                home.join(format!("skills/{name}/SKILL.md")),
                "old legacy skill",
            )
            .unwrap();
        }

        extract_bundled_files(home);

        for &(filename, _) in BUNDLED_FILES {
            assert_ne!(
                std::fs::read_to_string(home.join(filename)).unwrap(),
                "old",
                "{filename} was not re-extracted after version bump"
            );
        }
        assert_ne!(
            std::fs::read_to_string(home.join("skills/help/SKILL.md")).unwrap(),
            "old"
        );
        for name in ["check-work", "code-review"] {
            assert_ne!(
                std::fs::read_to_string(home.join(format!("skills/{name}/SKILL.md"))).unwrap(),
                "old",
                "{name} skill was not re-extracted after version bump"
            );
        }

        for name in ["check", "best-of-n", "docx", "pptx", "xlsx", "imagine"] {
            assert!(
                !home.join(format!("skills/{name}")).exists(),
                "legacy '{name}' skill directory should have been deleted during version bump"
            );
        }
    }

    #[test]
    fn office_skills_not_bundled() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        extract_bundled_files(home);

        // Former office document skills must NOT be extracted as bundled.
        for name in ["docx", "pptx", "xlsx"] {
            assert!(
                !home.join(format!("skills/{name}")).exists(),
                "{name} should not be a bundled skill"
            );
        }
    }

    #[tokio::test]
    async fn help_skill_discovered_by_skill_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        extract_bundled_files(home);

        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(workspace.join(".kigi").join("skills").join("help")).unwrap();
        std::fs::copy(
            home.join("skills/help/SKILL.md"),
            workspace.join(".kigi/skills/help/SKILL.md"),
        )
        .unwrap();

        let skills = kigi_agent::prompt::skills::list_skills(
            Some(workspace.to_str().unwrap()),
            &Default::default(),
            kigi_agent::prompt::skills::CompatConfig::default(),
        )
        .await;

        let help = skills.iter().find(|s| s.name == "help");
        assert!(
            help.is_some(),
            "help skill not found. skills: {:?}",
            skills.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        let help = help.unwrap();
        assert!(help.description.contains("configuration"));
        assert!(help.user_invocable);
    }

    #[test]
    fn remove_legacy_deletes_old_skill_when_not_currently_shipped() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        // Simulate an old legacy "check" directory from before a rename.
        let legacy_dir = home.join("skills/check");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(legacy_dir.join("SKILL.md"), "old check").unwrap();

        // "check" is in legacy list but NOT in current BUNDLED_SKILLS
        remove_legacy_skills(home, &["check"], BUNDLED_SKILLS);

        assert!(
            !legacy_dir.exists(),
            "legacy skill directory should have been deleted"
        );
    }

    #[test]
    fn remove_legacy_does_not_delete_when_name_is_reused_in_current_bundled() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        // User still has an old "check" directory.
        let legacy_dir = home.join("skills/check");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(legacy_dir.join("SKILL.md"), "user had old check").unwrap();

        // Simulate the situation where we later re-ship a skill named "check".
        // In this case the legacy entry should be ignored.
        let fake_bundled: &[(&str, &str)] = &[("check", "fake content"), ("help", "help")];

        remove_legacy_skills(home, &["check"], fake_bundled);

        // The directory must still exist (we did not nuke the user's copy
        // or a skill we're about to (re)create).
        assert!(
            legacy_dir.exists(),
            "should not delete a name that is currently being shipped"
        );
    }

    #[test]
    fn remove_legacy_handles_multiple_names_some_current_some_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        std::fs::create_dir_all(home.join("skills/old-renamed")).unwrap();
        std::fs::write(home.join("skills/old-renamed/SKILL.md"), "old").unwrap();

        std::fs::create_dir_all(home.join("skills/another-legacy")).unwrap();
        std::fs::write(home.join("skills/another-legacy/SKILL.md"), "old2").unwrap();

        // One currently-bundled name is also listed as legacy.
        let current: &[(&str, &str)] = &[("another-legacy", "now shipping again")];

        // Legacy list contains both the truly removed one and the reintroduced one
        remove_legacy_skills(home, &["old-renamed", "another-legacy"], current);

        assert!(
            !home.join("skills/old-renamed").exists(),
            "truly legacy name should be removed"
        );
        assert!(
            home.join("skills/another-legacy").exists(),
            "reintroduced name must not be deleted"
        );
    }

    #[test]
    fn remove_legacy_is_noop_when_directory_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        // No directory exists for the legacy name
        remove_legacy_skills(home, &["check"], BUNDLED_SKILLS);

        // Should not panic or create anything
        assert!(!home.join("skills/check").exists());
    }

    #[test]
    fn legacy_cleanup_runs_even_on_same_version_fast_path() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        // First run: extract current state
        extract_bundled_files(home);

        // Simulate user still having an old legacy directory
        let legacy_dir = home.join("skills/check");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(legacy_dir.join("SKILL.md"), "stale").unwrap();

        // Force the "same version" fast path by writing the current version marker
        let version = kigi_version::VERSION;
        std::fs::write(home.join(".metadata_version"), version).unwrap();

        // This should still run legacy cleanup even though we're in fast path
        extract_bundled_files(home);

        assert!(
            !legacy_dir.exists(),
            "legacy cleanup must run even on same-version fast path"
        );
    }
}
