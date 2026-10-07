//! `/model` (alias `/m`) — switch model + (optionally) reasoning effort.
//! Chained autocomplete: pick a reasoning-supported model → trailing space
//! re-opens the dropdown into a `low|medium|high|xhigh` sub-menu.

use agent_client_protocol as acp;
use kigi_shell::sampling::types::supports_reasoning_effort_meta;

use crate::acp::model_state::{ModelState, model_family, model_variant_name, native_fusion};
use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};
use crate::slash::commands::effort_levels::build_effort_arg_items;

/// Switch the active model (and optionally its reasoning effort).
pub struct ModelCommand;

impl SlashCommand for ModelCommand {
    fn name(&self) -> &str {
        "model"
    }

    fn aliases(&self) -> &[&str] {
        &["m"]
    }

    fn description(&self) -> &str {
        "Switch the active model"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn offered_when_session_less(&self) -> bool {
        // The dashboard offers `/model` to pick the model for the next
        // spawned agent (intercepted in `dispatch_dashboard_dispatch_slash`).
        true
    }

    fn usage(&self) -> &str {
        "/model <name> [effort]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("<model> [effort]")
    }

    fn suggest_args(&self, ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        if ctx.models.is_empty() {
            return None;
        }

        if let Some(family_id) = detect_family_phase(ctx.models, args_query) {
            return Some(build_family_items(ctx.models, &family_id));
        }

        // Effort phase if input is "<reasoning-model> ", else model phase.
        if let Some(model_id) = detect_effort_phase(ctx.models, args_query) {
            return Some(build_effort_items(ctx.models, &model_id));
        }
        Some(build_model_items(ctx.models))
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return CommandResult::Error("Usage: /model <name> [effort]".into());
        }

        // Prefer an exact full-string catalog match first. Model display names
        // often contain spaces ("Kigi 4.5"); if we split on the last token
        // first, a shorter catalog entry ("Kigi") would steal the prefix and
        // treat "4.5" as an effort level.
        if let Some(id) = ctx.models.resolve_by_name_or_id(trimmed) {
            return CommandResult::Action(Action::SetDefaultModel(id));
        }

        // Trailing effort token + reasoning model → session-scoped switch
        // (not persisted as default). Resolve via the shared gate so a rejected
        // level (e.g. `none` on kigi-4.5) surfaces the effort error with the
        // model's offered ids — not "Unknown model: … none".
        if let Some((prefix, token)) = split_trailing_token(trimmed)
            && let Some(id) = resolve_model(ctx.models, prefix)
            && ctx
                .models
                .available
                .get(&id)
                .map(supports_reasoning_effort)
                .unwrap_or(false)
        {
            return match ctx.models.resolve_effort_for_model(&id, token) {
                Ok(effort) => CommandResult::Action(Action::SwitchModel {
                    model_id: id,
                    effort: Some(effort),
                }),
                Err(err) => CommandResult::Error(err.message()),
            };
        }

        CommandResult::Error(format!("Unknown model: {trimmed}"))
    }
}

/// Look up a model by case-insensitive display name OR model id match.
fn resolve_model(models: &ModelState, name: &str) -> Option<acp::ModelId> {
    models.resolve_by_name_or_id(name)
}

fn supports_reasoning_effort(info: &acp::ModelInfo) -> bool {
    supports_reasoning_effort_meta(info.meta.as_ref())
}

/// Split `args` into `(prefix, last_token)` on the final whitespace run.
/// Returns `None` when there is no interior whitespace to split on. The token is
/// resolved to an effort against the picked model's options by the caller.
fn split_trailing_token(args: &str) -> Option<(&str, &str)> {
    let (prefix, last) = args.rsplit_once(char::is_whitespace)?;
    let prefix = prefix.trim_end();
    if prefix.is_empty() || last.is_empty() {
        return None;
    }
    Some((prefix, last))
}

/// Returns the matched model id when `args_query` is `"<reasoning-model> ..."`.
/// Longest-name-first to disambiguate names that share a prefix.
fn detect_effort_phase(models: &ModelState, args_query: &str) -> Option<acp::ModelId> {
    let mut candidates: Vec<(&acp::ModelId, &str)> = models
        .available
        .iter()
        .filter(|(_, info)| supports_reasoning_effort(info))
        .map(|(id, info)| (id, info.name.as_str()))
        .collect();
    candidates.sort_by_key(|(_, name)| std::cmp::Reverse(name.len()));

    for (id, name) in candidates {
        if args_query.len() > name.len()
            && args_query.is_char_boundary(name.len())
            && args_query[..name.len()].eq_ignore_ascii_case(name)
            && args_query[name.len()..].starts_with(char::is_whitespace)
        {
            return Some(id.clone());
        }
    }
    None
}

fn detect_family_phase(models: &ModelState, args_query: &str) -> Option<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut best: Option<(usize, String)> = None;
    for info in models.available.values() {
        if native_fusion(info).is_some() {
            continue;
        }
        let Some(family) = model_family(info) else {
            continue;
        };
        if !seen.insert(family.id.clone()) {
            continue;
        }
        for token in [
            family.name.clone(),
            format!("devin/{}", family.id),
            format!("devin/{}", family.name),
        ] {
            if args_query.len() > token.len()
                && args_query.is_char_boundary(token.len())
                && args_query[..token.len()].eq_ignore_ascii_case(&token)
                && args_query[token.len()..].starts_with(char::is_whitespace)
                && best.as_ref().is_none_or(|(len, _)| token.len() > *len)
            {
                best = Some((token.len(), family.id.clone()));
            }
        }
    }
    best.map(|(_, family_id)| family_id)
}

fn build_family_items(models: &ModelState, family_id: &str) -> Vec<ArgItem> {
    let current_id = models.current.as_ref();
    let mut items: Vec<ArgItem> = Vec::new();
    for (id, info) in &models.available {
        if native_fusion(info).is_some() {
            continue;
        }
        let Some(family) = model_family(info) else {
            continue;
        };
        if family.id != family_id {
            continue;
        }
        let variant = model_variant_name(info, &family);
        let mut display = variant.to_string();
        if current_id == Some(id) {
            display.push_str(" (current)");
        }
        if family.is_default {
            display.push_str(" (default)");
        }
        items.push(ArgItem {
            match_text: format!(
                "{} devin/{} devin/{} {}",
                family.name, family.id, family.name, info.name
            ),
            display,
            insert_text: id.0.to_string(),
            description: String::new(),
        });
    }
    items
}

/// One row per logical model. Reasoning models get a trailing space in
/// `insert_text` so the prompt widget chains into the effort sub-menu.
/// The description column names the model's provider (shell-stamped
/// `meta.provider` — the connected platform the model was fetched from)
/// when the model carries no description of its own.
fn build_model_items(models: &ModelState) -> Vec<ArgItem> {
    let current_id = models.current.as_ref();
    let mut member_counts: indexmap::IndexMap<String, usize> = indexmap::IndexMap::new();
    for info in models.available.values() {
        if native_fusion(info).is_some() {
            continue;
        }
        if let Some(family) = model_family(info) {
            *member_counts.entry(family.id).or_default() += 1;
        }
    }
    let mut emitted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut items: Vec<ArgItem> = Vec::with_capacity(models.available.len());
    for (id, info) in &models.available {
        if native_fusion(info).is_some() {
            continue;
        }
        if let Some(family) = model_family(info)
            && member_counts.get(&family.id).copied().unwrap_or(0) > 1
        {
            if emitted.insert(family.id.clone()) {
                let any_current = models.available.iter().any(|(mid, minfo)| {
                    current_id == Some(mid)
                        && native_fusion(minfo).is_none()
                        && model_family(minfo).is_some_and(|f| f.id == family.id)
                });
                items.push(ArgItem {
                    display: if any_current {
                        format!("{} (current)", family.name)
                    } else {
                        family.name.clone()
                    },
                    match_text: format!(
                        "{} devin/{} devin/{}",
                        family.name, family.id, family.name
                    ),
                    insert_text: format!("devin/{} ", family.id),
                    description: "Devin · model variants".to_string(),
                });
            }
            continue;
        }
        let is_current = current_id == Some(id);
        let supports = supports_reasoning_effort(info);

        let display = if is_current {
            format!("{} (current)", info.name)
        } else {
            info.name.clone()
        };

        // Trailing space on reasoning models: signals "more input
        // expected" to the prompt widget so Enter advances to effort
        // phase instead of submitting.
        let insert_text = if supports {
            format!("{} ", info.name)
        } else {
            info.name.clone()
        };

        let provider = info
            .meta
            .as_ref()
            .and_then(|m| m.get("provider"))
            .and_then(|v| v.as_str());
        let description = match (info.description.as_deref(), provider) {
            (Some(d), _) if !d.is_empty() => d.to_string(),
            (_, Some(p)) => p.to_string(),
            _ => String::new(),
        };

        items.push(ArgItem {
            display,
            match_text: info.name.clone(),
            insert_text,
            description,
        });
    }
    items
}

/// One row per effort level for the `/model` chained effort phase.
/// `insert_text` is `"ModelName high"` so selecting a row completes both tokens.
fn build_effort_items(models: &ModelState, model_id: &acp::ModelId) -> Vec<ArgItem> {
    let info = match models.available.get(model_id) {
        Some(info) => info,
        None => return Vec::new(),
    };
    let model_name = info.name.clone();
    let is_current_model = models.current.as_ref() == Some(model_id);
    let options = models.reasoning_effort_options_for(model_id);
    build_effort_arg_items(
        &options,
        models.reasoning_effort,
        is_current_model,
        |option| format!("{model_name} {}", option.id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kigi_shell::sampling::types::ReasoningEffort;
    use std::sync::Arc;

    fn model_with_reasoning(id: &str, name: &str) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let mut meta = serde_json::Map::new();
        meta.insert(
            "supportsReasoningEffort".into(),
            serde_json::Value::Bool(true),
        );
        let info = acp::ModelInfo::new(id.clone(), name.to_string())
            .meta(serde_json::Value::Object(meta).as_object().cloned());
        (id, info)
    }

    fn plain_model(id: &str, name: &str) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let info = acp::ModelInfo::new(id.clone(), name.to_string());
        (id, info)
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
            pager_state: crate::settings::PagerLocalSnapshot {
                multiline_mode: false,
                yolo_mode: false,
                ..crate::settings::PagerLocalSnapshot::default()
            },
        }
    }

    #[test]
    fn split_trailing_token_splits_on_final_whitespace() {
        assert_eq!(
            split_trailing_token("Reasoning X high"),
            Some(("Reasoning X", "high"))
        );
        assert_eq!(
            split_trailing_token("reasoning-x  xhigh"),
            Some(("reasoning-x", "xhigh"))
        );
        // No interior whitespace → nothing to split off.
        assert!(split_trailing_token("reasoning-x-pro").is_none());
    }

    #[test]
    fn empty_query_returns_one_row_per_logical_model() {
        let mut state = ModelState::default();
        let (rid, rinfo) = model_with_reasoning("reasoning-x", "Reasoning X");
        let (pid, pinfo) = plain_model("kigi-4.5", "Kigi 4.5");
        state.available.insert(rid, rinfo);
        state.available.insert(pid, pinfo);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        let items = cmd.suggest_args(&ctx, "").unwrap();
        assert_eq!(items.len(), 2, "model phase: one row per logical model");

        // Reasoning model has trailing space in insert_text -- this is the
        // signal the prompt widget reads to keep the dropdown open after
        // Enter so the effort sub-menu can render.
        let reasoning = items
            .iter()
            .find(|i| i.match_text == "Reasoning X")
            .unwrap();
        assert_eq!(reasoning.insert_text, "Reasoning X ");

        // Plain model has no trailing space -- Enter commits immediately.
        let plain = items.iter().find(|i| i.match_text == "Kigi 4.5").unwrap();
        assert_eq!(plain.insert_text, "Kigi 4.5");
    }

    /// Rows for shell-managed platform models show their provider (stamped
    /// `meta.provider`) in the description column, so the picker says which
    /// connected provider each model belongs to. A model's own description
    /// wins when present; entries without provider meta stay blank.
    #[test]
    fn model_rows_show_provider_in_description() {
        let mut state = ModelState::default();

        let claude_id = acp::ModelId::new(Arc::from("claude-pro-max/claude-opus-4-8"));
        let mut meta = serde_json::Map::new();
        meta.insert(
            "provider".into(),
            serde_json::Value::String("Claude Pro/Max".into()),
        );
        let claude =
            acp::ModelInfo::new(claude_id.clone(), "Claude Opus 4.8".to_string()).meta(Some(meta));
        state.available.insert(claude_id, claude);

        let (plain_id, plain_info) = plain_model("my-custom", "My Custom");
        state.available.insert(plain_id, plain_info);

        let items = build_model_items(&state);
        let claude_row = items
            .iter()
            .find(|i| i.match_text == "Claude Opus 4.8")
            .unwrap();
        assert_eq!(claude_row.description, "Claude Pro/Max");
        let plain_row = items.iter().find(|i| i.match_text == "My Custom").unwrap();
        assert_eq!(plain_row.description, "");
    }

    #[test]
    fn trailing_space_after_reasoning_model_enters_effort_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        // Args query has a trailing space -> effort phase. Items come out
        // ordered xhigh -> low (strongest first) per EFFORT_LEVELS.
        let items = cmd.suggest_args(&ctx, "Reasoning X ").unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].insert_text, "Reasoning X xhigh");
        assert_eq!(items[1].insert_text, "Reasoning X high");
        assert_eq!(items[2].insert_text, "Reasoning X medium");
        assert_eq!(items[3].insert_text, "Reasoning X low");
        // Display is just the level so the user sees a clean column.
        assert_eq!(items[0].display, "xhigh");
        // match_text carries the sort-key prefix that forces the matcher's
        // alphabetical tiebreak to render rows in EFFORT_LEVELS order.
        assert!(items[0].match_text.starts_with("a "));
        assert!(items[3].match_text.starts_with("d "));
    }

    #[test]
    fn partial_effort_query_still_in_effort_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        // Still in effort phase; matcher upstream narrows to high / xhigh.
        let items = cmd.suggest_args(&ctx, "Reasoning X h").unwrap();
        assert_eq!(items.len(), 4);
    }

    #[test]
    fn partial_model_query_stays_in_model_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        // No trailing space, user is still typing the model name.
        let items = cmd.suggest_args(&ctx, "Reason").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].insert_text, "Reasoning X ");
    }

    #[test]
    fn run_parses_model_plus_effort_when_supported() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Reasoning X xhigh");
        match result {
            CommandResult::Action(Action::SwitchModel { model_id, effort }) => {
                assert_eq!(model_id.0.as_ref(), "reasoning-x");
                assert_eq!(effort, Some(ReasoningEffort::Xhigh));
            }
            other => panic!("expected SwitchModel with effort, got {other:?}"),
        }
    }

    #[test]
    fn run_rejects_unoffered_effort_with_effort_error_not_unknown_model() {
        // Guards against a rejected effort token falling through to
        // `Unknown model: Reasoning X none` instead of an effort error.
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Reasoning X none");
        match result {
            CommandResult::Error(msg) => {
                assert!(
                    msg.contains("unknown effort level 'none'"),
                    "expected effort error, got {msg}"
                );
                assert!(
                    msg.contains("use one of:"),
                    "expected offered levels in message, got {msg}"
                );
                assert!(
                    !msg.to_lowercase().contains("unknown model"),
                    "must not misreport as unknown model: {msg}"
                );
                let offered = msg.split_once("; ").map(|(_, r)| r).unwrap_or("");
                assert!(
                    !offered.contains("none"),
                    "must not list none as offered: {msg}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn run_prefers_full_multi_word_model_name_over_prefix_plus_effort() {
        // Catalog has both "Kigi" (reasoning) and "Kigi 4.5". `/model Kigi 4.5`
        // must select the full name, not treat "4.5" as an effort on "Kigi".
        let mut state = ModelState::default();
        let (short_id, short_info) = model_with_reasoning("kigi", "Kigi");
        let (long_id, long_info) = model_with_reasoning("kigi-4.5", "Kigi 4.5");
        state.available.insert(short_id, short_info);
        state.available.insert(long_id.clone(), long_info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Kigi 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, long_id);
            }
            other => panic!("expected SetDefaultModel(Kigi 4.5), got {other:?}"),
        }
    }

    #[test]
    fn run_rejects_effort_for_non_reasoning_model() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("kigi-4.5", "Kigi 4.5");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Kigi 4.5 high");
        // Falls through to "is the whole string a model name?" — which
        // it isn't, so we get an Unknown error.
        assert!(matches!(result, CommandResult::Error(_)));
    }

    /// The bare `/model <name>` form dispatches
    /// `Action::SetDefaultModel(<ModelId>)` instead of the legacy
    /// `Action::SwitchModel { effort: None }`. The dispatcher routes
    /// the typed setter through both `Effect::SwitchModel`
    /// (session-level mutation) AND `Effect::PersistSetting`
    /// (next-session default).
    ///
    /// The payload is the typed `acp::ModelId` (resolved at the slash
    /// boundary), not a String.
    #[test]
    fn run_bare_model_name_dispatches_set_default_model() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("kigi-4.5", "Kigi 4.5");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Kigi 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, id);
            }
            other => panic!("expected Action::SetDefaultModel(<id>), got {other:?}"),
        }
    }

    fn family_model(
        id: &str,
        name: &str,
        family_id: &str,
        family_name: &str,
        is_default: bool,
    ) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let info = acp::ModelInfo::new(id.clone(), name.to_string()).meta(
            serde_json::json!({
                "modelFamily": {"id": family_id, "name": family_name, "isDefault": is_default},
            })
            .as_object()
            .cloned(),
        );
        (id, info)
    }

    fn app_ctx(models: &ModelState) -> AppCtx<'_> {
        AppCtx {
            models,
            cwd: std::path::Path::new("."),
            screen_mode: crate::app::ScreenMode::Fullscreen,
        }
    }

    #[test]
    fn root_phase_one_row_per_devin_family_and_variants_hidden() {
        let mut state = ModelState::default();
        for (id, name, def) in [
            ("devin/swe-2-high", "SWE-2 High", false),
            ("devin/swe-2-medium", "SWE-2 Medium", true),
            ("devin/swe-2-high-fast", "SWE-2 High Fast", false),
            ("devin/opus-medium", "Claude Opus 5.5 Medium", false),
            ("devin/opus-max", "Claude Opus 5.5 Max", true),
        ] {
            let (fid, finfo) = if id.contains("opus") {
                family_model(id, name, "claude-opus-5-5", "Claude Opus 5.5", def)
            } else {
                family_model(id, name, "swe-2", "SWE-2", def)
            };
            state.available.insert(fid, finfo);
        }
        let (pid, pinfo) = plain_model("kigi-4.5", "Kigi 4.5");
        state.available.insert(pid, pinfo);

        let cmd = ModelCommand;
        let ctx = app_ctx(&state);
        let items = cmd.suggest_args(&ctx, "").unwrap();
        let displays: Vec<&str> = items.iter().map(|i| i.display.as_str()).collect();
        assert_eq!(
            displays,
            vec!["SWE-2", "Claude Opus 5.5", "Kigi 4.5"],
            "one root row per multi-member family; variants hidden"
        );
        let swe = &items[0];
        assert_eq!(swe.insert_text, "devin/swe-2 ");
        assert_eq!(swe.description, "Devin · model variants");
        assert!(swe.match_text.contains("SWE-2"));
        assert!(swe.match_text.contains("devin/swe-2"));
    }

    #[test]
    fn family_phase_after_root_token_lists_every_variant_with_real_uid() {
        let mut state = ModelState::default();
        for (id, name, def) in [
            ("devin/swe-2-high", "SWE-2 High", false),
            ("devin/swe-2-medium", "SWE-2 Medium", true),
            ("devin/swe-2-high-fast", "SWE-2 High Fast", false),
        ] {
            let (fid, finfo) = family_model(id, name, "swe-2", "SWE-2", def);
            state.available.insert(fid, finfo);
        }
        let cmd = ModelCommand;
        let ctx = app_ctx(&state);

        for query in ["devin/swe-2 ", "SWE-2 ", "devin/SWE-2 hi"] {
            let items = cmd.suggest_args(&ctx, query).unwrap();
            assert_eq!(items.len(), 3, "query {query}: all variants offered");
            let inserts: Vec<&str> = items.iter().map(|i| i.insert_text.as_str()).collect();
            assert_eq!(
                inserts,
                vec![
                    "devin/swe-2-high",
                    "devin/swe-2-medium",
                    "devin/swe-2-high-fast"
                ],
                "query {query}: children insert the actual catalog uid"
            );
            assert_eq!(items[0].display, "High");
            assert_eq!(items[1].display, "Medium (default)");
            assert_eq!(items[2].display, "High Fast");
        }
    }

    #[test]
    fn family_run_resolves_name_to_default_variant_and_exact_id_stays() {
        let mut state = ModelState::default();
        for (id, name, def) in [
            ("devin/swe-2-high", "SWE-2 High", false),
            ("devin/swe-2-medium", "SWE-2 Medium", true),
        ] {
            let (fid, finfo) = family_model(id, name, "swe-2", "SWE-2", def);
            state.available.insert(fid, finfo);
        }
        let mut ctx = dummy_exec_ctx(&state);
        match ModelCommand.run(&mut ctx, "SWE-2") {
            CommandResult::Action(Action::SetDefaultModel(id)) => {
                assert_eq!(id.0.as_ref(), "devin/swe-2-medium");
            }
            other => panic!("family name resolves default variant, got {other:?}"),
        }
        match ModelCommand.run(&mut ctx, "devin/swe-2-high") {
            CommandResult::Action(Action::SetDefaultModel(id)) => {
                assert_eq!(id.0.as_ref(), "devin/swe-2-high");
            }
            other => panic!("concrete uid resolves itself, got {other:?}"),
        }
        match ModelCommand.run(&mut ctx, "SWE-2 High") {
            CommandResult::Action(Action::SetDefaultModel(id)) => {
                assert_eq!(id.0.as_ref(), "devin/swe-2-high");
            }
            other => panic!("full variant name resolves itself, got {other:?}"),
        }
    }

    #[test]
    fn family_phase_prefers_longest_overlapping_token() {
        let mut state = ModelState::default();
        let (a, ai) = family_model("devin/swe-2-high", "SWE-2 High", "swe-2", "SWE-2", false);
        let (b, bi) = family_model(
            "devin/swe-2-pro-max",
            "SWE-2 Pro Max",
            "swe-2-pro",
            "SWE-2 Pro",
            false,
        );
        state.available.insert(a, ai);
        state.available.insert(b, bi);
        let cmd = ModelCommand;
        let ctx = app_ctx(&state);
        let items = cmd.suggest_args(&ctx, "SWE-2 Pro m").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].insert_text, "devin/swe-2-pro-max");
    }

    #[test]
    fn family_root_shows_current_when_a_variant_is_active() {
        let mut state = ModelState::default();
        let (a, ai) = family_model("devin/swe-2-high", "SWE-2 High", "swe-2", "SWE-2", false);
        let (b, bi) = family_model("devin/swe-2-max", "SWE-2 Max", "swe-2", "SWE-2", true);
        state.available.insert(a.clone(), ai);
        state.available.insert(b, bi);
        state.current = Some(a);
        let items = build_model_items(&state);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].display, "SWE-2 (current)");
    }

    #[test]
    fn singleton_family_rows_stay_flat() {
        let mut state = ModelState::default();
        let (a, ai) = family_model("devin/adaptive", "Adaptive", "adaptive", "Adaptive", true);
        state.available.insert(a, ai);
        let items = build_model_items(&state);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].insert_text, "Adaptive");
        assert_eq!(items[0].display, "Adaptive");
    }

    /// Case-insensitive matching against the catalog: `/model kigi 4.5`
    /// resolves to the same `ModelId` as `/model Kigi 4.5`.
    #[test]
    fn run_set_default_model_resolves_case_insensitively() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("kigi-4.5", "Kigi 4.5");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "kigi 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, id);
            }
            other => panic!("expected Action::SetDefaultModel(<id>), got {other:?}"),
        }
    }
}
