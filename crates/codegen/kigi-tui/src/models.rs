//! `kigi models` subcommand.

use agent_client_protocol as acp;
use anyhow::Result;
use kigi_shell::agent::config::Config as AgentConfig;
use kigi_shell::cli_models::{AuthStatus, list_models};
use tokio_util::sync::CancellationToken;

use crate::client_identity::{PAGER_CLIENT_TYPE, PAGER_CLIENT_VERSION};

pub async fn list_available_models(agent_config: &AgentConfig) -> Result<()> {
    match AuthStatus::resolve(agent_config) {
        AuthStatus::ApiKey => println!("You are using KIGI_API_KEY."),
        AuthStatus::LoggedIn(host) => println!("You are logged in with {}.", host),
        AuthStatus::ModelCredentials(model) => {
            println!("Model '{model}' is using its own API key.");
        }
        AuthStatus::DeploymentKey => println!("You are authenticated via deployment key."),
        AuthStatus::NotAuthenticated => println!("You are not authenticated."),
    }
    println!();

    let cancel = CancellationToken::new();
    let spawned = crate::acp::spawn::spawn_kigi_shell(agent_config.clone(), &cancel, None).await?;

    let state = list_models(&spawned.channel.tx, PAGER_CLIENT_TYPE, PAGER_CLIENT_VERSION).await?;

    println!("Default model: {}", state.current_model_id.0);
    println!();
    println!("Available models:");
    for line in format_available_models(&state.available_models, &state.current_model_id) {
        println!("{line}");
    }

    cancel.cancel();
    Ok(())
}

fn format_available_models(models: &[acp::ModelInfo], current: &acp::ModelId) -> Vec<String> {
    let mut members: indexmap::IndexMap<String, Vec<usize>> = indexmap::IndexMap::new();
    for (i, m) in models.iter().enumerate() {
        if let Some(f) = crate::acp::model_state::model_family(m) {
            members.entry(f.id).or_default().push(i);
        }
    }
    let grouped: std::collections::HashSet<String> = members
        .iter()
        .filter(|(_, idx)| idx.len() > 1)
        .map(|(fid, _)| fid.clone())
        .collect();
    let mut done: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut lines = Vec::with_capacity(models.len());
    for (i, m) in models.iter().enumerate() {
        if done.contains(&i) {
            continue;
        }
        if let Some(f) = crate::acp::model_state::model_family(m) {
            if grouped.contains(&f.id) {
                lines.push(format!("  {} (Devin)", f.name));
                for &mi in &members[&f.id] {
                    done.insert(mi);
                    let member = &models[mi];
                    let variant = crate::acp::model_state::model_variant_name(member, &f);
                    if member.model_id == *current {
                        lines.push(format!(
                            "    * {} [{}] (default)",
                            variant, member.model_id.0
                        ));
                    } else {
                        lines.push(format!("    - {} [{}]", variant, member.model_id.0));
                    }
                }
                continue;
            }
            if m.model_id == *current {
                lines.push(format!("  * {} [{}] (default)", m.name, m.model_id.0));
            } else {
                lines.push(format!("  - {} [{}]", m.name, m.model_id.0));
            }
            continue;
        }
        if m.model_id == *current {
            lines.push(format!("  * {} (default)", m.model_id.0));
        } else {
            lines.push(format!("  - {}", m.model_id.0));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn model(id: &str, name: &str) -> acp::ModelInfo {
        acp::ModelInfo::new(acp::ModelId::new(Arc::from(id)), name.to_string())
    }

    fn family_model(
        id: &str,
        name: &str,
        family_id: &str,
        family_name: &str,
        is_default: bool,
    ) -> acp::ModelInfo {
        model(id, name).meta(
            serde_json::json!({
                "modelFamily": {"id": family_id, "name": family_name, "isDefault": is_default},
            })
            .as_object()
            .cloned(),
        )
    }

    #[test]
    fn cli_models_groups_devin_families_and_keeps_every_uid() {
        let current = acp::ModelId::new(Arc::from("devin/swe-2-medium"));
        let models = vec![
            family_model("devin/swe-2-high", "SWE-2 High", "swe-2", "SWE-2", false),
            family_model("devin/swe-2-medium", "SWE-2 Medium", "swe-2", "SWE-2", true),
            family_model("devin/swe-2-max", "SWE-2 Max", "swe-2", "SWE-2", false),
            model("kimi-code/k2", "K2"),
        ];
        let lines = format_available_models(&models, &current);
        assert_eq!(
            lines,
            vec![
                "  SWE-2 (Devin)",
                "    - High [devin/swe-2-high]",
                "    * Medium [devin/swe-2-medium] (default)",
                "    - Max [devin/swe-2-max]",
                "  - kimi-code/k2",
            ],
            "family header, all uids preserved, current flagged, non-family flat"
        );
    }

    #[test]
    fn cli_models_interleaved_families_emit_one_header_each() {
        let current = acp::ModelId::new(Arc::from("devin/swe-2-high"));
        let models = vec![
            family_model("devin/a-x", "FamA X", "a", "FamA", false),
            family_model("devin/b-y", "FamB Y", "b", "FamB", false),
            family_model("devin/a-z", "FamA Z", "a", "FamA", false),
            family_model("devin/b-q", "FamB Q", "b", "FamB", false),
        ];
        let lines = format_available_models(&models, &current);
        let headers = lines.iter().filter(|l| !l.starts_with("    ")).count();
        assert_eq!(headers, 2, "interleaved members never duplicate a header");
        let fam_a: Vec<&String> = lines
            .iter()
            .skip_while(|l| l.as_str() != "  FamA (Devin)")
            .take(3)
            .collect();
        assert_eq!(fam_a.len(), 3);
        assert!(fam_a[1].contains("devin/a-x"));
        assert!(fam_a[2].contains("devin/a-z"));
    }

    #[test]
    fn cli_models_absent_or_foreign_family_meta_stays_flat() {
        let current = acp::ModelId::new(Arc::from("other/x"));
        let foreign = family_model("other/x", "X", "fam", "Fam", true);
        let models = vec![foreign, model("devin/plain", "Plain")];
        let lines = format_available_models(&models, &current);
        assert_eq!(
            lines,
            vec!["  * other/x (default)", "  - devin/plain"],
            "foreign provider meta and family-less devin ids render flat"
        );
    }

    #[test]
    fn cli_models_singleton_devin_family_shows_readable_name_and_uid() {
        let current = acp::ModelId::new(Arc::from("devin/adaptive"));
        let models = vec![
            family_model("devin/adaptive", "Adaptive", "adaptive", "Adaptive", true),
            model("kimi-code/k2", "K2"),
        ];
        let lines = format_available_models(&models, &current);
        assert_eq!(
            lines,
            vec![
                "  * Adaptive [devin/adaptive] (default)",
                "  - kimi-code/k2"
            ],
            "singleton devin family reads as name + uid, not a bare uid"
        );
        let other = acp::ModelId::new(Arc::from("kimi-code/k2"));
        let lines = format_available_models(&models, &other);
        assert_eq!(lines[0], "  - Adaptive [devin/adaptive]");
    }
}
