//! Skill discovery reminder — discovers new skills near accessed paths.
//!
//! The tracking logic lives in
//! `types::skill_discovery_tracker::SkillDiscoveryTracker`.

use std::path::{Path, PathBuf};

/// Config directories whose `skills/` subdirectory holds skill definitions.
/// Shared between startup discovery and the runtime `SkillDiscoveryReminder`.
pub const SKILL_CONFIG_DIRS: &[&str] = &[".kigi", ".agents", ".claude", ".cursor"];

use crate::implementations::skills::discovery;
use crate::implementations::skills::types::SkillScope;
use crate::types::output::{
    ApplyPatchOutput, ListDirOutput, ReadFileOutput, SearchReplaceOutput, ToolOutput,
};
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::SharedResources;
use crate::types::skill_discovery_tracker::SkillManager;
use crate::types::tool::{Reminder, ToolKind};

/// Cross-cutting reminder that discovers skills in directories near the
/// filesystem paths tools access. Register it alongside tools so it runs after
/// every tool call.
///
/// **Concise mode limitation (V1):** this reminder is globally disabled when
/// `SystemRemindersEnabled(false)` is set (concise mode), so dynamic skill
/// discovery does NOT fire in concise mode. This is an intentional V1 layering
/// compromise — discovery is coupled to the Reminder delivery mechanism for
/// expediency. If concise-mode support is later needed, migrate to a dedicated
/// post-tool-call hook that is NOT gated by `SystemRemindersEnabled`.
pub struct SkillDiscoveryReminder;

impl SkillDiscoveryReminder {
    /// The filesystem path the tool accessed, or `None` for non-filesystem
    /// tools and error variants (no reliable path to extract).
    fn extract_target_path(tool_output: &ToolOutput) -> Option<&Path> {
        match tool_output {
            ToolOutput::ReadFile(ReadFileOutput::FileContent(fc)) => Some(&fc.absolute_path),
            ToolOutput::ListDir(ListDirOutput::Content(content)) => {
                Some(&content.absolute_root_path)
            }
            ToolOutput::SearchReplace(SearchReplaceOutput::EditsApplied(r)) => {
                Some(&r.absolute_path)
            }
            _ => None,
        }
    }

    /// Files the tool touched, for activating `paths:`-gated skills — including
    /// every file of a multi-file `apply_patch`. Bash/grep paths are excluded
    /// (unparseable / incidental).
    fn extract_activation_paths(tool_output: &ToolOutput) -> Vec<PathBuf> {
        match tool_output {
            ToolOutput::ApplyPatch(ApplyPatchOutput::Success { files, .. }) => files
                .iter()
                .flat_map(|f| std::iter::once(f.path.clone()).chain(f.move_to.clone()))
                .collect(),
            other => Self::extract_target_path(other)
                .map(Path::to_path_buf)
                .into_iter()
                .collect(),
        }
    }

    fn is_in_supported_skills_dir(path: &Path) -> bool {
        for ancestor in path.ancestors().skip(1) {
            if ancestor.file_name().is_some_and(|n| n == "skills") {
                return ancestor
                    .parent()
                    .and_then(|p| p.file_name())
                    .is_some_and(|n| SKILL_CONFIG_DIRS.iter().any(|d| *d == n));
            }
        }
        false
    }
}

#[async_trait::async_trait]
impl Reminder for SkillDiscoveryReminder {
    fn requires_expr(&self) -> Expr<ToolRequirement> {
        // Finalization-time check: "at least one path-producing tool exists."
        // At runtime, collect_reminders fires after every tool call regardless
        // — output pattern-matching does the actual filtering.
        Expr::Or(vec![
            Expr::Value(ToolRequirement::tool_kind(ToolKind::Read)),
            Expr::Value(ToolRequirement::tool_kind(ToolKind::Edit)),
            Expr::Value(ToolRequirement::tool_kind(ToolKind::List)),
        ])
    }

    async fn collect_reminders(
        &self,
        resources: SharedResources,
        tool_output: &ToolOutput,
    ) -> Vec<String> {
        // Activate `paths:`-gated skills for every file the tool touched.
        let activation_paths = Self::extract_activation_paths(tool_output);
        if !activation_paths.is_empty() {
            let path_refs: Vec<&Path> = activation_paths.iter().map(PathBuf::as_path).collect();
            let mut res = resources.lock().await;
            if let Some(tracker) = res.get_mut::<SkillManager>() {
                tracker.activate_conditional_skills_for_paths(&path_refs);
            }
        }

        // Discovery itself walks from a single representative path.
        let Some(target_path) = Self::extract_target_path(tool_output) else {
            return vec![];
        };

        // Direct SKILL.md detection: when a tool writes (or reads) a
        // SKILL.md file, register it immediately. The normal upward-walk
        // discovery cannot find these because it looks for `.kigi/skills/`
        // sub-directories in *ancestor* dirs, and user-scope skills
        // (~/.kigi/) are outside the git root so the walk breaks early.
        if target_path.file_name().is_some_and(|n| n == "SKILL.md")
            && Self::is_in_supported_skills_dir(target_path)
        {
            let scope = {
                let res = resources.lock().await;
                let tracker = res.get::<SkillManager>();
                let cwd = tracker.and_then(|m| m.cwd.clone());
                let git_root = tracker.and_then(|m| m.git_root.clone());
                match (cwd, git_root) {
                    (Some(cwd), _) if target_path.starts_with(&cwd) => SkillScope::Local,
                    (_, Some(root)) if target_path.starts_with(&root) => SkillScope::Repo,
                    _ => SkillScope::User,
                }
            };
            let skills = discovery::parse_skill_files(vec![(target_path.to_path_buf(), scope)]);
            if !skills.is_empty() {
                let mut res = resources.lock().await;
                if let Some(tracker) = res.get_mut::<SkillManager>() {
                    tracker.add_discovered(skills);
                }
            }
            return vec![];
        }

        // Snapshot context under the lock, then release it before doing I/O.
        let (cwd, git_root, mut checked_dirs_snapshot, compat) = {
            let res = resources.lock().await;
            let Some(tracker) = res.get::<SkillManager>() else {
                return vec![];
            };
            let cwd = match tracker.cwd.clone() {
                Some(c) => c,
                None => return vec![],
            };
            (
                cwd,
                tracker.git_root.clone(),
                tracker.checked_dirs.clone(),
                tracker.compat,
            )
        };

        // Filesystem discovery runs outside the lock.
        let discovered = discovery::discover_skills_for_paths(
            &[target_path],
            &cwd,
            git_root.as_deref(),
            &mut checked_dirs_snapshot,
            compat,
        );

        if discovered.is_empty() {
            // Even if no skills found, merge checked_dirs back so we don't
            // re-stat the same directories on future calls.
            let mut res = resources.lock().await;
            if let Some(tracker) = res.get_mut::<SkillManager>() {
                tracker.checked_dirs.extend(checked_dirs_snapshot);
            }
            return vec![];
        }

        // Merge results into the tracker. This reminder never returns
        // announcement text itself; the session drains announcements from the
        // tracker via take_pending_reconciliation() after each tool call.
        {
            let mut res = resources.lock().await;
            let tracker = match res.get_mut::<SkillManager>() {
                Some(t) => t,
                None => return vec![],
            };

            tracker.checked_dirs.extend(checked_dirs_snapshot);

            // add_discovered sets the pending flag the session later drains.
            tracker.add_discovered(discovered);
        }

        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::output::ApplyPatchFileResult;

    fn edited(path: &str, move_to: Option<&str>) -> ApplyPatchFileResult {
        ApplyPatchFileResult {
            path: PathBuf::from(path),
            action: "modified".into(),
            old_text: None,
            new_text: String::new(),
            move_to: move_to.map(PathBuf::from),
        }
    }

    #[test]
    fn apply_patch_activation_paths_cover_every_edited_file() {
        let multi_file_patch = ToolOutput::ApplyPatch(ApplyPatchOutput::Success {
            files: vec![edited("/r/a.rs", None), edited("/r/b.rs", Some("/r/c.rs"))],
            tool_output_for_prompt: String::new(),
        });
        assert_eq!(
            SkillDiscoveryReminder::extract_activation_paths(&multi_file_patch),
            vec![
                PathBuf::from("/r/a.rs"),
                PathBuf::from("/r/b.rs"),
                PathBuf::from("/r/c.rs"),
            ],
        );
        let failed_patch = ToolOutput::ApplyPatch(ApplyPatchOutput::ParseError("x".into()));
        assert!(SkillDiscoveryReminder::extract_activation_paths(&failed_patch).is_empty());
    }
}
