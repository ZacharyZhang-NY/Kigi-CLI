use std::io::Read as _;

use anyhow::Context as _;
use kigi_models::WireModel;
use kigi_sampling_types::devin;

use super::models_fetch::BackendError;

fn devin_blocking_client() -> Result<reqwest::blocking::Client, BackendError> {
    reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("failed to build devin catalog client")
        .map_err(|e| BackendError::Auth(e.to_string()))
}

pub(crate) fn fetch_devin_models(
    platform: kigi_models::PlatformId,
    bearer: &str,
) -> Result<Vec<WireModel>, BackendError> {
    let base = platform.base_url();
    let url = format!(
        "{}{}",
        base.trim_end_matches('/'),
        devin::GET_CLI_MODEL_CONFIGS_PATH
    );
    let api_key = devin::normalize_devin_session_token(bearer);
    let body = devin::build_get_cli_model_configs_request(
        &api_key,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default(),
        &uuid::Uuid::new_v4().to_string(),
    );

    let resp = devin_blocking_client()?
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, devin::PROTO_CONTENT_TYPE)
        .header(
            devin::CONNECT_PROTOCOL_VERSION_HEADER,
            devin::CONNECT_PROTOCOL_VERSION,
        )
        .header(reqwest::header::ACCEPT, devin::PROTO_CONTENT_TYPE)
        .body(body)
        .send()
        .map_err(|e| {
            if let Some(status) = e.status() {
                BackendError::RequestFailed {
                    status: status.as_u16(),
                    body: "devin catalog request failed".to_string(),
                }
            } else {
                BackendError::Network(e)
            }
        })?;

    let status = resp.status();
    if !status.is_success() {
        drop(resp);
        return Err(BackendError::RequestFailed {
            status: status.as_u16(),
            body: "devin catalog request failed".to_string(),
        });
    }

    let mut bytes = Vec::new();
    resp.take((devin::MAX_DEVIN_UNARY_PAYLOAD + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| BackendError::Auth(format!("devin catalog read failed: {e}")))?;
    if bytes.len() > devin::MAX_DEVIN_UNARY_PAYLOAD {
        return Err(BackendError::RequestFailed {
            status: 200,
            body: "devin catalog response oversized".to_string(),
        });
    }
    let decoded: devin::GetCliModelConfigsResponse =
        devin::decode_unary(&bytes).map_err(|e| BackendError::RequestFailed {
            status: 200,
            body: format!("devin catalog decode failed: {e}"),
        })?;
    devin_configs_to_wire_models(decoded)
}

fn concrete_wire_model(cfg: &devin::ClientModelConfig) -> Option<WireModel> {
    if cfg.disabled {
        return None;
    }
    let uid = cfg.model_uid.trim();
    if uid.is_empty() {
        return None;
    }
    let info = cfg.model_info.as_ref();
    if info.is_some_and(|i| i.is_model_router) {
        return None;
    }
    let features = info.and_then(|i| i.model_features.as_ref());
    if features.is_some_and(|f| !f.supports_tool_calls) {
        return None;
    }
    let context = info
        .map(|i| i.max_tokens)
        .filter(|v| *v > 0)
        .or_else(|| (cfg.max_tokens > 0).then_some(cfg.max_tokens))
        .map(|v| v as u64)
        .unwrap_or(crate::agent::models_fetch::DEFAULT_CONTEXT_WINDOW);
    let max_output = info
        .map(|i| i.max_output_tokens)
        .filter(|v| *v > 0)
        .map(|v| v as u64)
        .unwrap_or_else(|| context.min(64_000));
    let model_family = cfg.model_family_metadata.as_ref().and_then(|m| {
        let name = m.model_family_label.trim();
        (!name.is_empty()).then(|| {
            let uid_key = info
                .map(|i| i.model_family_uid.trim())
                .filter(|u| !u.is_empty())
                .unwrap_or(name);
            kigi_models::ModelFamilyInfo {
                id: uid_key.to_string(),
                name: name.to_string(),
                is_default: cfg.is_default_model_in_family || m.is_default_model_in_family,
            }
        })
    });
    let label = cfg.label.trim();
    Some(WireModel {
        id: uid.to_string(),
        context_length: context,
        supports_reasoning: features.is_some_and(|f| f.supports_thinking),
        supports_image_in: features
            .map(|f| f.supports_images)
            .unwrap_or(cfg.supports_images),
        supports_video_in: false,
        display_name: (!label.is_empty()).then(|| label.to_string()),
        max_output_tokens: max_output,
        supports_thinking_type: None,
        think_efforts: None,
        model_family,
        fusion: None,
    })
}

fn fusion_family(cfg: &devin::ClientModelConfig) -> kigi_models::ModelFamilyInfo {
    if let Some(m) = cfg.model_family_metadata.as_ref() {
        let name = m.model_family_label.trim();
        if !name.is_empty() {
            let uid_key = cfg
                .model_info
                .as_ref()
                .map(|i| i.model_family_uid.trim())
                .filter(|u| !u.is_empty())
                .unwrap_or(name);
            return kigi_models::ModelFamilyInfo {
                id: uid_key.to_string(),
                name: name.to_string(),
                is_default: cfg.is_default_model_in_family || m.is_default_model_in_family,
            };
        }
    }
    kigi_models::ModelFamilyInfo {
        id: "fusion".to_string(),
        name: "Fusion".to_string(),
        is_default: cfg.is_default_model_in_family,
    }
}

/// The concrete model a fusion row's lead segment names. GPT speed variants
/// are spelled `-fast` inside a router uid but `-priority` as a concrete uid.
fn fusion_lead<'a>(
    concrete: &'a std::collections::HashMap<String, WireModel>,
    lead_uid: &str,
) -> Option<&'a WireModel> {
    concrete.get(lead_uid).or_else(|| {
        let base = lead_uid.strip_suffix("-fast")?;
        concrete.get(&format!("{base}-priority"))
    })
}

pub(crate) fn devin_configs_to_wire_models(
    response: devin::GetCliModelConfigsResponse,
) -> Result<Vec<WireModel>, BackendError> {
    let mut concrete: std::collections::HashMap<String, WireModel> =
        std::collections::HashMap::new();
    for cfg in &response.client_model_configs {
        let uid = cfg.model_uid.trim();
        if uid.starts_with("fusion-") {
            continue;
        }
        if let Some(wire) = concrete_wire_model(cfg) {
            concrete.entry(uid.to_string()).or_insert(wire);
        }
    }
    let mut emitted = std::collections::HashSet::new();
    let mut out = Vec::new();
    for cfg in &response.client_model_configs {
        let uid = cfg.model_uid.trim();
        if uid.is_empty() || cfg.disabled {
            continue;
        }
        if let Some((lead_uid, helper_uid)) = devin::fusion_model_uids(uid) {
            let (Some(lead_wire), Some(helper_wire)) =
                (fusion_lead(&concrete, lead_uid), concrete.get(helper_uid))
            else {
                continue;
            };
            if !emitted.insert(uid.to_string()) {
                continue;
            }
            let lead_label = lead_wire
                .display_name
                .clone()
                .unwrap_or_else(|| lead_uid.to_string());
            let helper_label = helper_wire
                .display_name
                .clone()
                .unwrap_or_else(|| helper_uid.to_string());
            let label = cfg.label.trim();
            let mut wire = lead_wire.clone();
            wire.id = uid.to_string();
            wire.display_name = Some(if label.is_empty() {
                format!("Fusion ({lead_label} + {helper_label})")
            } else {
                label.to_string()
            });
            wire.fusion = Some(kigi_models::ModelFusionInfo {
                lead: lead_label,
                sidekick: helper_label,
                lead_model: lead_uid.to_string(),
                sidekick_model: helper_uid.to_string(),
            });
            wire.model_family = Some(fusion_family(cfg));
            out.push(wire);
            continue;
        }
        if uid.starts_with("fusion-") {
            continue;
        }
        if concrete_wire_model(cfg).is_some()
            && let Some(wire) = concrete.get(uid)
            && emitted.insert(uid.to_string())
        {
            out.push(wire.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    fn config(uid: &str) -> devin::ClientModelConfig {
        devin::ClientModelConfig {
            model_uid: uid.to_string(),
            ..Default::default()
        }
    }

    fn featured(
        uid: &str,
        context: i32,
        max_out: i32,
        tool_calls: bool,
        thinking: bool,
        images: bool,
    ) -> devin::ClientModelConfig {
        devin::ClientModelConfig {
            model_uid: uid.to_string(),
            model_info: Some(devin::ModelInfo {
                max_tokens: context,
                max_output_tokens: max_out,
                model_features: Some(devin::ModelFeatures {
                    supports_images: images,
                    supports_tool_calls: tool_calls,
                    supports_thinking: thinking,
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn catalog_filters_disabled_blank_router_and_no_tool_models() {
        let router = devin::ClientModelConfig {
            model_uid: "MODEL_ROUTER".into(),
            model_info: Some(devin::ModelInfo {
                is_model_router: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let resp = devin::GetCliModelConfigsResponse {
            client_model_configs: vec![
                featured("MODEL_GOOD", 200_000, 8_000, true, true, true),
                devin::ClientModelConfig {
                    disabled: true,
                    ..featured("MODEL_DISABLED", 1, 1, true, false, false)
                },
                config("   "),
                config(""),
                router,
                featured("MODEL_NO_TOOLS", 1, 1, false, false, false),
                featured("MODEL_DUP", 1, 1, true, false, false),
                devin::ClientModelConfig {
                    label: "dup-2".into(),
                    ..featured("MODEL_DUP", 9, 9, true, false, false)
                },
            ],
        };
        let models = devin_configs_to_wire_models(resp).expect("project");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["MODEL_GOOD", "MODEL_DUP"], "only kept rows");
        let good = &models[0];
        assert_eq!(good.context_length, 200_000);
        assert_eq!(good.max_output_tokens, 8_000);
        assert!(good.supports_reasoning);
        assert!(good.supports_image_in);
    }

    #[test]
    fn catalog_nested_metadata_wins_and_absent_features_invent_nothing() {
        let nested = devin::ClientModelConfig {
            model_uid: "MODEL_NESTED".into(),
            max_tokens: 999,
            model_info: Some(devin::ModelInfo {
                max_tokens: 131_072,
                max_output_tokens: -1,
                ..Default::default()
            }),
            ..Default::default()
        };
        let plain = devin::ClientModelConfig {
            model_uid: "MODEL_PLAIN".into(),
            supports_images: true,
            max_tokens: 65_000,
            ..Default::default()
        };
        let resp = devin::GetCliModelConfigsResponse {
            client_model_configs: vec![nested, plain],
        };
        let models = devin_configs_to_wire_models(resp).expect("project");
        assert_eq!(models[0].context_length, 131_072, "nested context wins");
        assert_eq!(models[0].max_output_tokens, 64_000);
        assert!(!models[0].supports_reasoning, "no invented thinking");
        assert!(!models[0].supports_image_in, "features say nothing → false");
        assert_eq!(models[1].context_length, 65_000);
        assert!(models[1].supports_image_in, "config flag carries images");
        assert!(!models[1].supports_reasoning);
    }

    #[test]
    fn catalog_gzipped_unary_response_decodes() {
        let resp = devin::GetCliModelConfigsResponse {
            client_model_configs: vec![featured("MODEL_GZIP", 1000, 100, true, false, false)],
        };
        let wire = devin::gunzip_bounded(&resp.encode_to_vec(), devin::MAX_DEVIN_UNARY_PAYLOAD);
        assert!(wire.is_err(), "bare bytes are not gzip");
        let gz = {
            use std::io::Write as _;
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(&resp.encode_to_vec()).unwrap();
            e.finish().unwrap()
        };
        let decoded: devin::GetCliModelConfigsResponse = devin::decode_unary(&gz).expect("gunzip");
        let models = devin_configs_to_wire_models(decoded).expect("project");
        assert_eq!(models[0].id, "MODEL_GZIP");
    }

    fn with_family(
        uid: &str,
        label: &str,
        family_uid: &str,
        family_label: &str,
        cfg_default: bool,
        meta_default: bool,
    ) -> devin::ClientModelConfig {
        devin::ClientModelConfig {
            model_uid: uid.to_string(),
            label: label.to_string(),
            is_default_model_in_family: cfg_default,
            model_info: Some(devin::ModelInfo {
                model_family_uid: family_uid.to_string(),
                ..Default::default()
            }),
            model_family_metadata: Some(devin::ModelFamilyMetadata {
                model_family_label: family_label.to_string(),
                is_default_model_in_family: meta_default,
                entries: vec![],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn family_members_all_stay_selectable_and_default_comes_from_either_flag() {
        let resp = devin::GetCliModelConfigsResponse {
            client_model_configs: vec![
                with_family("MODEL_SW_HIGH", "SWE-2 High", "swe-2", "SWE-2", false, true),
                with_family(
                    "MODEL_SW_MED",
                    "SWE-2 Medium",
                    "swe-2",
                    "SWE-2",
                    true,
                    false,
                ),
                with_family("MODEL_SW_MAX", "SWE-2 Max", "swe-2", "SWE-2", false, false),
            ],
        };
        let models = devin_configs_to_wire_models(resp).expect("project");
        assert_eq!(models.len(), 3, "every variant stays selectable");
        assert!(models.iter().all(|m| {
            m.model_family
                .as_ref()
                .is_some_and(|f| f.id == "swe-2" && f.name == "SWE-2")
        }));
        assert!(models[0].model_family.as_ref().unwrap().is_default);
        assert!(models[1].model_family.as_ref().unwrap().is_default);
        assert!(!models[2].model_family.as_ref().unwrap().is_default);
    }

    #[test]
    fn family_uid_falls_back_to_label_and_blank_metadata_yields_none() {
        let mut no_uid = with_family("MODEL_A", "Opus A", "", "  Opus 5  ", false, false);
        no_uid.label = " Opus A ".into();
        let no_meta = devin::ClientModelConfig {
            model_uid: "MODEL_B".into(),
            ..Default::default()
        };
        let blank_meta = devin::ClientModelConfig {
            model_uid: "MODEL_C".into(),
            model_family_metadata: Some(devin::ModelFamilyMetadata {
                model_family_label: "   ".into(),
                entries: vec![],
                is_default_model_in_family: false,
            }),
            ..Default::default()
        };
        let resp = devin::GetCliModelConfigsResponse {
            client_model_configs: vec![no_uid, no_meta, blank_meta],
        };
        let models = devin_configs_to_wire_models(resp).expect("project");
        let fam = models[0].model_family.as_ref().expect("family present");
        assert_eq!(fam.id, "Opus 5", "blank family_uid falls back to label");
        assert_eq!(fam.name, "Opus 5", "label trimmed");
        assert_eq!(models[0].display_name.as_deref(), Some("Opus A"));
        assert!(models[1].model_family.is_none(), "absent metadata → None");
        assert!(models[2].model_family.is_none(), "blank label → None");
    }

    #[test]
    fn families_sharing_a_name_prefix_do_not_merge() {
        let resp = devin::GetCliModelConfigsResponse {
            client_model_configs: vec![
                with_family("MODEL_SW_A", "SWE-2 A", "swe-2", "SWE-2", false, false),
                with_family(
                    "MODEL_SWP_B",
                    "SWE-2 Pro B",
                    "swe-2-pro",
                    "SWE-2 Pro",
                    false,
                    false,
                ),
            ],
        };
        let models = devin_configs_to_wire_models(resp).expect("project");
        assert_eq!(models[0].model_family.as_ref().unwrap().id, "swe-2");
        assert_eq!(models[1].model_family.as_ref().unwrap().id, "swe-2-pro");
        assert_ne!(
            models[0].model_family.as_ref().unwrap().id,
            models[1].model_family.as_ref().unwrap().id
        );
    }

    fn fusion_router(uid: &str, label: &str) -> devin::ClientModelConfig {
        devin::ClientModelConfig {
            model_uid: uid.to_string(),
            label: label.to_string(),
            model_info: Some(devin::ModelInfo {
                is_model_router: true,
                model_features: Some(devin::ModelFeatures {
                    supports_tool_calls: false,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn labelled_featured(uid: &str, label: &str, tool_calls: bool) -> devin::ClientModelConfig {
        devin::ClientModelConfig {
            label: label.to_string(),
            ..featured(uid, 200_000, 8_000, tool_calls, true, false)
        }
    }

    #[test]
    fn catalog_fusion_pair_emitted_with_lead_capabilities_and_pair_dto() {
        let resp = devin::GetCliModelConfigsResponse {
            client_model_configs: vec![
                fusion_router(
                    "fusion-claude-test-medium-sidekick-swe-test-medium",
                    "Fusion (Claude Test Medium + SWE Test Medium)",
                ),
                labelled_featured("claude-test-medium", "Claude Test Medium", true),
                labelled_featured("swe-test-medium", "SWE Test Medium", true),
            ],
        };
        let models = devin_configs_to_wire_models(resp).expect("project");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "fusion-claude-test-medium-sidekick-swe-test-medium",
                "claude-test-medium",
                "swe-test-medium",
            ],
            "pair row precedes still-selectable component rows"
        );
        let pair = &models[0];
        let fusion = pair.fusion.as_ref().expect("fusion dto");
        assert_eq!(fusion.lead, "Claude Test Medium");
        assert_eq!(fusion.sidekick, "SWE Test Medium");
        assert_eq!(fusion.lead_model, "claude-test-medium");
        assert_eq!(fusion.sidekick_model, "swe-test-medium");
        assert_eq!(pair.context_length, 200_000, "lead capabilities cloned");
        assert_eq!(pair.max_output_tokens, 8_000);
        assert!(pair.supports_reasoning);
        assert_eq!(
            pair.display_name.as_deref(),
            Some("Fusion (Claude Test Medium + SWE Test Medium)")
        );
        let family = pair.model_family.as_ref().expect("fusion family");
        assert_eq!(
            (family.id.as_str(), family.name.as_str()),
            ("fusion", "Fusion")
        );
        assert!(models[1].fusion.is_none() && models[2].fusion.is_none());
    }

    #[test]
    fn catalog_fusion_fast_lead_resolves_the_priority_concrete() {
        let resp = devin::GetCliModelConfigsResponse {
            client_model_configs: vec![
                fusion_router(
                    "fusion-gpt-test-high-fast-sidekick-swe-test-medium",
                    "Fusion (GPT Test High Fast + SWE Test Medium)",
                ),
                fusion_router("fusion-absent-fast-sidekick-swe-test-medium", "Orphan"),
                labelled_featured("gpt-test-high-priority", "GPT Test High Fast", true),
                labelled_featured("swe-test-medium", "SWE Test Medium", true),
            ],
        };
        let models = devin_configs_to_wire_models(resp).expect("project");
        let pairs: Vec<&WireModel> = models.iter().filter(|m| m.fusion.is_some()).collect();
        assert_eq!(
            pairs.len(),
            1,
            "a lead with no concrete in either spelling is dropped"
        );
        let fusion = pairs[0].fusion.as_ref().expect("fusion dto");
        assert_eq!(fusion.lead, "GPT Test High Fast");
        assert_eq!(
            fusion.lead_model, "gpt-test-high-fast",
            "the pair keeps the router's own spelling, which the subagent guard re-parses"
        );
    }

    #[test]
    fn catalog_fusion_pair_uses_provider_family_meta_when_present() {
        let mut router = fusion_router(
            "fusion-claude-test-medium-sidekick-swe-test-medium",
            "Custom Fusion",
        );
        router.model_family_metadata = Some(devin::ModelFamilyMetadata {
            model_family_label: "Router Family".to_string(),
            ..Default::default()
        });
        router.is_default_model_in_family = true;
        let resp = devin::GetCliModelConfigsResponse {
            client_model_configs: vec![
                router,
                labelled_featured("claude-test-medium", "Claude Test Medium", true),
                labelled_featured("swe-test-medium", "SWE Test Medium", true),
            ],
        };
        let models = devin_configs_to_wire_models(resp).expect("project");
        let family = models[0].model_family.as_ref().unwrap();
        assert_eq!(family.name, "Router Family");
        assert_eq!(family.id, "Router Family");
        assert!(family.is_default);
    }

    #[test]
    fn catalog_fusion_pair_requires_both_concrete_components() {
        let component_cases = vec![
            labelled_featured("claude-test-medium", "Claude Test Medium", true),
            devin::ClientModelConfig {
                disabled: true,
                ..labelled_featured("swe-test-medium", "SWE Test Medium", true)
            },
        ];
        for extra in [
            component_cases.clone(),
            vec![devin::ClientModelConfig {
                model_info: Some(devin::ModelInfo {
                    is_model_router: true,
                    model_features: Some(devin::ModelFeatures {
                        supports_tool_calls: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..labelled_featured("swe-test-medium", "SWE Test Medium", true)
            }],
            vec![],
        ] {
            let mut cfgs = vec![
                fusion_router(
                    "fusion-claude-test-medium-sidekick-swe-test-medium",
                    "Fusion pair",
                ),
                labelled_featured("claude-test-medium", "Claude Test Medium", true),
            ];
            cfgs.extend(extra);
            let resp = devin::GetCliModelConfigsResponse {
                client_model_configs: cfgs,
            };
            let models = devin_configs_to_wire_models(resp).expect("project");
            assert!(
                !models.iter().any(|m| m.id.starts_with("fusion-")),
                "disabled/router/missing helper components never emit a pair"
            );
        }
    }

    #[test]
    fn catalog_fusion_pair_rejects_no_tool_components() {
        for (lead_tools, helper_tools) in [(false, true), (true, false)] {
            let resp = devin::GetCliModelConfigsResponse {
                client_model_configs: vec![
                    fusion_router(
                        "fusion-claude-test-medium-sidekick-swe-test-medium",
                        "Fusion pair",
                    ),
                    labelled_featured("claude-test-medium", "Claude Test Medium", lead_tools),
                    labelled_featured("swe-test-medium", "SWE Test Medium", helper_tools),
                ],
            };
            let models = devin_configs_to_wire_models(resp).expect("project");
            assert!(
                !models.iter().any(|m| m.id.starts_with("fusion-")),
                "no-tool lead or helper component never emits a pair"
            );
        }
    }

    #[test]
    fn catalog_fusion_duplicate_uid_dedups_and_blank_label_synthesizes() {
        let mut unlabelled =
            fusion_router("fusion-claude-test-medium-sidekick-swe-test-medium", "   ");
        unlabelled.is_default_model_in_family = true;
        let resp = devin::GetCliModelConfigsResponse {
            client_model_configs: vec![
                unlabelled,
                fusion_router("fusion-claude-test-medium-sidekick-swe-test-medium", "dup"),
                labelled_featured("claude-test-medium", "Claude Test Medium", true),
                labelled_featured("swe-test-medium", "SWE Test Medium", true),
                devin::ClientModelConfig {
                    model_info: Some(devin::ModelInfo {
                        is_model_router: true,
                        ..Default::default()
                    }),
                    ..config("adaptive")
                },
            ],
        };
        let models = devin_configs_to_wire_models(resp).expect("project");
        let fusion_rows: Vec<&WireModel> = models
            .iter()
            .filter(|m| m.id.starts_with("fusion-"))
            .collect();
        assert_eq!(fusion_rows.len(), 1, "duplicate fusion uid emitted once");
        assert_eq!(
            fusion_rows[0].display_name.as_deref(),
            Some("Fusion (Claude Test Medium + SWE Test Medium)")
        );
        assert!(fusion_rows[0].model_family.as_ref().unwrap().is_default);
        assert!(
            !models.iter().any(|m| m.id == "adaptive"),
            "general routers stay excluded"
        );
    }

    #[test]
    fn catalog_first_eligible_server_order_and_malformed_fusion_excluded() {
        let resp = devin::GetCliModelConfigsResponse {
            client_model_configs: vec![
                devin::ClientModelConfig {
                    disabled: true,
                    ..fusion_router(
                        "fusion-claude-test-medium-sidekick-swe-test-medium",
                        "disabled dup pair",
                    )
                },
                labelled_featured("ordinary-x", "Ordinary X", true),
                fusion_router(
                    "fusion-claude-test-medium-sidekick-swe-test-medium",
                    "Fusion (Claude Test Medium + SWE Test Medium)",
                ),
                labelled_featured("claude-test-medium", "Claude Test Medium", true),
                labelled_featured("swe-test-medium", "SWE Test Medium", true),
            ],
        };
        let models = devin_configs_to_wire_models(resp).expect("project");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "ordinary-x",
                "fusion-claude-test-medium-sidekick-swe-test-medium",
                "claude-test-medium",
                "swe-test-medium",
            ],
            "disabled first duplicate must not steal the valid pair's position"
        );

        let resp = devin::GetCliModelConfigsResponse {
            client_model_configs: vec![
                devin::ClientModelConfig {
                    disabled: true,
                    ..labelled_featured("ordinary-a", "Ordinary A", true)
                },
                labelled_featured("ordinary-x", "Ordinary X", true),
                labelled_featured("ordinary-a", "Ordinary A", true),
            ],
        };
        let models = devin_configs_to_wire_models(resp).expect("project");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["ordinary-x", "ordinary-a"]);

        let resp = devin::GetCliModelConfigsResponse {
            client_model_configs: vec![labelled_featured("fusion-unpaired", "Weird", true)],
        };
        let models = devin_configs_to_wire_models(resp).expect("project");
        assert!(
            models.is_empty(),
            "a malformed fusion-prefixed uid never falls through as an ordinary model"
        );
    }
}
