//! `/fusion` — pick a native Devin fusion pair (lead + sidekick).
//!
//! Native Devin fusion entries are managed `devin/fusion-<lead>-sidekick-<s>`
//! catalog ids whose `_meta.fusion` carries `{lead, sidekick, leadModel,
//! sidekickModel}`; `native_fusion` validates that metadata against the pair
//! uid itself. The picker is two-stage: root shows one row per lead model,
//! Enter expands that lead's sidekick list, and a child row inserts the exact
//! `devin/fusion-…` pair id — switching rides the same `Action::SwitchModel`
//! path as `/model`.

use agent_client_protocol as acp;

use crate::acp::model_state::{ModelState, native_fusion};
use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

/// Switch to a fusion lead+sidekick pair (native Devin).
pub struct FusionCommand;

impl SlashCommand for FusionCommand {
    fn name(&self) -> &str {
        "fusion"
    }

    fn description(&self) -> &str {
        "Switch to a fusion lead+sidekick pair"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn usage(&self) -> &str {
        "/fusion <lead[+sidekick]>"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("<lead> [sidekick]")
    }

    fn visible(&self, ctx: &AppCtx) -> bool {
        fusion_entries(ctx.models).next().is_some()
    }

    fn suggest_args(&self, ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        let models = ctx.models;
        let current_id = models.current.as_ref();

        if let Some(lead_model) = detect_native_fusion_phase(models, args_query) {
            let items: Vec<ArgItem> = models
                .available
                .iter()
                .filter_map(|(id, info)| native_fusion(info).map(|f| (id, f)))
                .filter(|(_, f)| f.lead_model == lead_model)
                .map(|(id, f)| ArgItem {
                    display: if current_id == Some(id) {
                        format!("{} (current)", f.sidekick)
                    } else {
                        f.sidekick.clone()
                    },
                    match_text: format!(
                        "{} devin/{} devin/{} {} {}",
                        f.lead, f.lead_model, f.lead, f.sidekick, id.0
                    ),
                    insert_text: id.0.to_string(),
                    description: format!("{} + {}", f.lead, f.sidekick),
                })
                .collect();
            return if items.is_empty() { None } else { Some(items) };
        }

        let mut items: Vec<ArgItem> = Vec::new();
        let mut seen_leads: std::collections::HashSet<String> = std::collections::HashSet::new();
        for info in models.available.values() {
            let Some(f) = native_fusion(info) else {
                continue;
            };
            if !seen_leads.insert(f.lead_model.clone()) {
                continue;
            }
            let any_current = models.available.iter().any(|(mid, minfo)| {
                current_id == Some(mid)
                    && native_fusion(minfo).is_some_and(|g| g.lead_model == f.lead_model)
            });
            items.push(ArgItem {
                display: if any_current {
                    format!("{} (current)", f.lead)
                } else {
                    f.lead.clone()
                },
                match_text: format!("{} devin/{}", f.lead, f.lead_model),
                insert_text: format!("devin/{} ", f.lead_model),
                description: "Devin · fusion sidekicks".to_string(),
            });
        }
        if items.is_empty() { None } else { Some(items) }
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            let pairs: Vec<String> = fusion_entries(ctx.models)
                .map(|(_, info, _)| info.name.clone())
                .collect();
            if pairs.is_empty() {
                return CommandResult::Error("No fusion pairs on this agent".into());
            }
            return CommandResult::Error(format!(
                "Usage: /fusion <pair> — available: {}",
                pairs.join(", ")
            ));
        }
        match resolve_fusion(ctx.models, trimmed) {
            Some(id) => CommandResult::Action(Action::SwitchModel {
                model_id: id,
                effort: None,
            }),
            None => {
                let pairs: Vec<String> = fusion_entries(ctx.models)
                    .map(|(_, info, _)| info.name.clone())
                    .collect();
                if pairs.is_empty() {
                    CommandResult::Error("No fusion pairs on this agent".into())
                } else {
                    CommandResult::Error(format!(
                        "Unknown fusion pair '{trimmed}' — available: {}",
                        pairs.join(", ")
                    ))
                }
            }
        }
    }
}

fn detect_native_fusion_phase(models: &ModelState, args_query: &str) -> Option<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut best: Option<(usize, String)> = None;
    for info in models.available.values() {
        let Some(f) = native_fusion(info) else {
            continue;
        };
        if !seen.insert(f.lead_model.clone()) {
            continue;
        }
        for token in [f.lead.clone(), format!("devin/{}", f.lead_model)] {
            if args_query.len() > token.len()
                && args_query.is_char_boundary(token.len())
                && args_query[..token.len()].eq_ignore_ascii_case(&token)
                && args_query[token.len()..].starts_with(char::is_whitespace)
                && best.as_ref().is_none_or(|(len, _)| token.len() > *len)
            {
                best = Some((token.len(), f.lead_model.clone()));
            }
        }
    }
    best.map(|(_, lead_model)| lead_model)
}

/// The native fusion pairs in the catalog — `(model_id, info, fusion dto)`.
fn fusion_entries(
    models: &ModelState,
) -> impl Iterator<
    Item = (
        &acp::ModelId,
        &acp::ModelInfo,
        kigi_shell::agent::config::ModelFusionInfo,
    ),
> {
    models
        .available
        .iter()
        .filter_map(|(id, info)| native_fusion(info).map(|f| (id, info, f)))
}

/// Match `arg` to a native fusion pair: exact `devin/fusion-…` id, full
/// display name (`Fusion (L + S)`), `devin/<lead-uid>` or bare lead label
/// (first pair for that lead), or `lead sidekick` / `lead + sidekick`
/// tokens (case-insensitive).
fn resolve_fusion(models: &ModelState, arg: &str) -> Option<acp::ModelId> {
    let normalized: Vec<(acp::ModelId, String, String, String)> = fusion_entries(models)
        .map(|(id, info, f)| {
            (
                id.clone(),
                info.name.to_lowercase(),
                f.lead.to_lowercase(),
                f.sidekick.to_lowercase(),
            )
        })
        .collect();
    let a = arg.to_lowercase();
    // Exact managed id (`devin/fusion-…`).
    if let Some((id, _, _, _)) = normalized.iter().find(|(id, _, _, _)| id.0.as_ref() == arg) {
        return Some(id.clone());
    }
    // Full display name.
    if let Some((id, _, _, _)) = normalized.iter().find(|(_, n, _, _)| *n == a) {
        return Some(id.clone());
    }
    // `lead sidekick` / `lead + sidekick` — compare against the name core.
    let core = a
        .strip_prefix("fusion (")
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(&a)
        .replace(" + ", " ");
    let mut lead_only: Option<&acp::ModelId> = None;
    for (id, _, lead, sidekick) in &normalized {
        if *lead == core {
            lead_only = lead_only.or(Some(id));
            continue;
        }
        if format!("{lead} {sidekick}") == core {
            return Some(id.clone());
        }
    }
    for (id, info) in &models.available {
        if let Some(f) = native_fusion(info)
            && format!("devin/{}", f.lead_model).eq_ignore_ascii_case(arg)
        {
            return Some(id.clone());
        }
    }
    lead_only.cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn native_pair(
        pair_uid: &str,
        lead: &str,
        sidekick: &str,
        lead_model: &str,
        sidekick_model: &str,
    ) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(format!("devin/{pair_uid}")));
        let info = acp::ModelInfo::new(id.clone(), format!("Fusion ({lead} + {sidekick})")).meta(
            serde_json::json!({
                "fusion": {
                    "lead": lead, "sidekick": sidekick,
                    "leadModel": lead_model, "sidekickModel": sidekick_model,
                },
            })
            .as_object()
            .cloned(),
        );
        (id, info)
    }

    fn fusion_models() -> ModelState {
        let mut state = ModelState::default();
        for (id, info) in [
            native_pair("fusion-a-sidekick-b", "A", "B", "a", "b"),
            native_pair("fusion-a-sidekick-c", "A", "C", "a", "c"),
            native_pair("fusion-d-sidekick-b", "D", "B", "d", "b"),
        ] {
            state.available.insert(id, info);
        }
        // A non-fusion entry must be ignored.
        let plain = acp::ModelId::new(Arc::from("devin/swe-2"));
        state.available.insert(
            plain.clone(),
            acp::ModelInfo::new(plain, "SWE-2".to_string()),
        );
        state
    }

    static EMPTY_BUNDLE: crate::app::bundle::BundleState = crate::app::bundle::BundleState {
        has_cache: false,
        version: String::new(),
        personas: Vec::new(),
        roles: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        persona_details: Vec::new(),
        role_details: Vec::new(),
    };

    fn dummy_exec_ctx(models: &ModelState) -> CommandExecCtx<'_> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: &EMPTY_BUNDLE,
            screen_mode: crate::app::ScreenMode::Inline,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn hidden_without_fusion_pairs() {
        let mut state = ModelState::default();
        let plain = acp::ModelId::new(Arc::from("devin/swe-2"));
        state.available.insert(
            plain.clone(),
            acp::ModelInfo::new(plain, "SWE-2".to_string()),
        );
        let cwd = std::path::PathBuf::from("/tmp");
        let ctx = AppCtx {
            models: &state,
            cwd: &cwd,
            screen_mode: crate::app::ScreenMode::Inline,
        };
        assert!(!FusionCommand.visible(&ctx));
        let models = fusion_models();
        let ctx = AppCtx {
            models: &models,
            cwd: &cwd,
            screen_mode: crate::app::ScreenMode::Inline,
        };
        assert!(FusionCommand.visible(&ctx));
    }

    #[test]
    fn native_fusion_root_rows_one_per_lead_with_trailing_space() {
        let models = fusion_models();
        let cwd = std::path::PathBuf::from("/tmp");
        let ctx = AppCtx {
            models: &models,
            cwd: &cwd,
            screen_mode: crate::app::ScreenMode::Inline,
        };
        let items = FusionCommand.suggest_args(&ctx, "").expect("items");
        let roots: Vec<&ArgItem> = items
            .iter()
            .filter(|i| i.insert_text.ends_with(' '))
            .collect();
        assert_eq!(roots.len(), 2, "one root row per lead model");
        assert_eq!(roots[0].display, "A");
        assert_eq!(roots[0].insert_text, "devin/a ");
        assert_eq!(roots[1].display, "D");
        assert_eq!(roots[1].insert_text, "devin/d ");
    }

    #[test]
    fn native_fusion_child_phase_lists_pair_uids() {
        let models = fusion_models();
        let cwd = std::path::PathBuf::from("/tmp");
        let ctx = AppCtx {
            models: &models,
            cwd: &cwd,
            screen_mode: crate::app::ScreenMode::Inline,
        };
        for prefix in ["devin/a ", "A "] {
            let items = FusionCommand.suggest_args(&ctx, prefix).expect("children");
            let inserts: Vec<&str> = items.iter().map(|i| i.insert_text.as_str()).collect();
            assert_eq!(
                inserts,
                vec!["devin/fusion-a-sidekick-b", "devin/fusion-a-sidekick-c"],
                "prefix {prefix:?} lists the actual pair uids"
            );
            assert_eq!(items[0].display, "B");
            assert!(
                items[0].match_text.contains("devin/a")
                    && items[0].match_text.contains("B")
                    && items[0].match_text.contains("A"),
                "match_text carries parent aliases + helper: {}",
                items[0].match_text
            );
        }
    }

    #[test]
    fn native_fusion_resolves_exact_lead_alias_and_full_name() {
        let models = fusion_models();
        for (q, want) in [
            ("devin/fusion-a-sidekick-c", "devin/fusion-a-sidekick-c"),
            ("Fusion (A + C)", "devin/fusion-a-sidekick-c"),
            ("a + c", "devin/fusion-a-sidekick-c"),
            ("a c", "devin/fusion-a-sidekick-c"),
            ("devin/a", "devin/fusion-a-sidekick-b"),
            ("a", "devin/fusion-a-sidekick-b"),
            ("devin/d", "devin/fusion-d-sidekick-b"),
        ] {
            assert_eq!(
                resolve_fusion(&models, q)
                    .map(|id| id.0.to_string())
                    .as_deref(),
                Some(want),
                "query: {q}"
            );
        }
    }

    #[test]
    fn ignores_non_fusion_and_unknown() {
        let models = fusion_models();
        assert_eq!(resolve_fusion(&models, "devin/swe-2"), None);
        assert_eq!(resolve_fusion(&models, "fusion-nope"), None);
        assert_eq!(resolve_fusion(&models, "a b c"), None);
    }

    #[test]
    fn native_fusion_phase_longest_overlapping_lead_prefix_and_case() {
        let mut models = fusion_models();
        for (id, info) in [
            native_pair("fusion-lead-sidekick-h1", "Lead", "H1", "lead", "h1"),
            native_pair(
                "fusion-lead-pro-sidekick-h1",
                "Lead Pro",
                "H1",
                "lead-pro",
                "h1",
            ),
            native_pair(
                "fusion-lead-pro-sidekick-h9",
                "Lead Pro",
                "H9",
                "lead-pro",
                "h9",
            ),
        ] {
            models.available.insert(id, info);
        }
        let cwd = std::path::PathBuf::from("/tmp");
        let ctx = AppCtx {
            models: &models,
            cwd: &cwd,
            screen_mode: crate::app::ScreenMode::Inline,
        };
        let items = FusionCommand
            .suggest_args(&ctx, "lead pro ")
            .expect("children");
        let inserts: Vec<&str> = items.iter().map(|i| i.insert_text.as_str()).collect();
        assert_eq!(
            inserts,
            vec![
                "devin/fusion-lead-pro-sidekick-h1",
                "devin/fusion-lead-pro-sidekick-h9"
            ],
            "longest lead alias wins: `lead pro ` beats the `lead` prefix"
        );

        let items = FusionCommand
            .suggest_args(&ctx, "DEVIN/LEAD-PRO ")
            .expect("children");
        let inserts: Vec<&str> = items.iter().map(|i| i.insert_text.as_str()).collect();
        assert_eq!(
            inserts.len(),
            2,
            "devin/<lead_uid> alias is case-insensitive"
        );
    }

    #[test]
    fn native_fusion_current_pair_marks_root_and_helper() {
        let mut models = fusion_models();
        let current = acp::ModelId::new(Arc::from("devin/fusion-a-sidekick-b"));
        models.current = Some(current);
        let cwd = std::path::PathBuf::from("/tmp");
        let ctx = AppCtx {
            models: &models,
            cwd: &cwd,
            screen_mode: crate::app::ScreenMode::Inline,
        };
        let items = FusionCommand.suggest_args(&ctx, "").expect("items");
        let lead_a = items.iter().find(|i| i.insert_text == "devin/a ").unwrap();
        assert_eq!(
            lead_a.display, "A (current)",
            "any active pair marks its lead"
        );
        let lead_d = items.iter().find(|i| i.insert_text == "devin/d ").unwrap();
        assert_eq!(lead_d.display, "D");

        let items = FusionCommand
            .suggest_args(&ctx, "devin/a ")
            .expect("children");
        let helper_b = items
            .iter()
            .find(|i| i.insert_text == "devin/fusion-a-sidekick-b")
            .unwrap();
        assert_eq!(helper_b.display, "B (current)");
        let helper_c = items
            .iter()
            .find(|i| i.insert_text == "devin/fusion-a-sidekick-c")
            .unwrap();
        assert_eq!(helper_c.display, "C");
    }

    #[test]
    fn native_fusion_run_emits_switch_model() {
        let mut models = fusion_models();
        let (id, info) = native_pair("fusion-e-sidekick-f", "LeadE", "HelperF", "e", "f");
        models.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&models);
        let out = FusionCommand.run(&mut ctx, "devin/fusion-e-sidekick-f");
        let model_id = match out {
            CommandResult::Action(Action::SwitchModel { model_id, .. }) => model_id,
            other => panic!("expected SwitchModel, got {other:?}"),
        };
        assert_eq!(model_id.0.as_ref(), "devin/fusion-e-sidekick-f");

        let out = FusionCommand.run(&mut ctx, "devin/e");
        assert_eq!(
            match out {
                CommandResult::Action(Action::SwitchModel { model_id, .. }) => {
                    model_id.0.to_string()
                }
                other => panic!("expected SwitchModel, got {other:?}"),
            },
            "devin/fusion-e-sidekick-f"
        );
    }
}
