#![cfg_attr(rustfmt, rustfmt::skip)]
use super::*;
fn with_env_var<T>(name: &str, value: &str, f: impl FnOnce() -> T) -> T {
    let previous = std::env::var(name).ok();
    unsafe {
        std::env::set_var(name, value);
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match previous {
        Some(prev) => {
            unsafe {
                std::env::set_var(name, prev);
            }
        }
        None => {
            unsafe {
                std::env::remove_var(name);
            }
        }
    }
    match result {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}
#[test]
fn expands_env_vars_in_toml_strings() {
    with_env_var(
        "KIGI_TEST_CONFIG_EXPAND",
        "expanded",
        || {
            let toml_str = r#"
[mcp_servers.test]
command = "$KIGI_TEST_CONFIG_EXPAND/bin/server"
args = ["--path", "${KIGI_TEST_CONFIG_EXPAND}/data"]
"#;
            let mut value = toml::from_str::<toml::Value>(toml_str).unwrap();
            expand_env_vars_in_toml(&mut value);
            let toml::Value::Table(table) = value else {
                panic!("Expected table root");
            };
            let Some(toml::Value::Table(mcp_servers)) = table.get("mcp_servers") else {
                panic!("Expected mcp_servers table");
            };
            let Some(toml::Value::Table(test)) = mcp_servers.get("test") else {
                panic!("Expected test table");
            };
            let command = test.get("command").and_then(|v| v.as_str()).unwrap();
            assert_eq!(command, "expanded/bin/server");
            let args = test.get("args").and_then(|v| v.as_array()).unwrap();
            assert_eq!(args[1].as_str().unwrap(), "expanded/data");
        },
    );
}
#[test]
fn leaves_missing_env_vars_unchanged() {
    let toml_str = r#"
[mcp_servers.test]
command = "$KIGI_TEST_CONFIG_MISSING/bin/server"
"#;
    let mut value = toml::from_str::<toml::Value>(toml_str).unwrap();
    expand_env_vars_in_toml(&mut value);
    let toml::Value::Table(table) = value else {
        panic!("Expected table root");
    };
    let Some(toml::Value::Table(mcp_servers)) = table.get("mcp_servers") else {
        panic!("Expected mcp_servers table");
    };
    let Some(toml::Value::Table(test)) = mcp_servers.get("test") else {
        panic!("Expected test table");
    };
    let command = test.get("command").and_then(|v| v.as_str()).unwrap();
    assert_eq!(command, "$KIGI_TEST_CONFIG_MISSING/bin/server");
}
#[test]
fn preserves_literal_dollar_signs() {
    let toml_str = r#"
[mcp_servers.test]
command = "$$HOME"
"#;
    let mut value = toml::from_str::<toml::Value>(toml_str).unwrap();
    expand_env_vars_in_toml(&mut value);
    let toml::Value::Table(table) = value else {
        panic!("Expected table root");
    };
    let Some(toml::Value::Table(mcp_servers)) = table.get("mcp_servers") else {
        panic!("Expected mcp_servers table");
    };
    let Some(toml::Value::Table(test)) = mcp_servers.get("test") else {
        panic!("Expected test table");
    };
    let command = test.get("command").and_then(|v| v.as_str()).unwrap();
    assert_eq!(command, "$HOME");
}
/// Mutex to serialize tests that touch the KIGI_MEMORY env var.
/// Env vars are process-global, so parallel tests race on them.
static MEMORY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Run `f` with `name` set to `value` (Some) or removed (None).
/// Saves and restores the previous value, even on panic.
fn with_env_var_opt<T>(name: &str, value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let previous = std::env::var(name).ok();
    match value {
        Some(v) => unsafe { std::env::set_var(name, v) }
        None => unsafe { std::env::remove_var(name) }
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match previous {
        Some(prev) => unsafe { std::env::set_var(name, prev) }
        None => unsafe { std::env::remove_var(name) }
    }
    result.unwrap_or_else(|p| std::panic::resume_unwind(p))
}
fn without_kigi_memory<T>(f: impl FnOnce() -> T) -> T {
    let _guard = MEMORY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_var_opt("KIGI_MEMORY", None, f)
}
fn with_kigi_memory<T>(value: &str, f: impl FnOnce() -> T) -> T {
    let _guard = MEMORY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_var_opt("KIGI_MEMORY", Some(value), f)
}
#[test]
fn memory_config_default_disabled() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(! mem.enabled);
    });
}
#[test]
fn memory_config_cli_flag_enables() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let mem = MemoryConfig::resolve(true, false, &config, None);
        assert!(mem.enabled);
    });
}
#[test]
fn memory_config_from_toml() {
    without_kigi_memory(|| {
        let config: toml::Value = toml::from_str("[memory]\nenabled = true").unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.enabled);
    });
}
#[test]
fn memory_config_toml_disabled() {
    without_kigi_memory(|| {
        let config: toml::Value = toml::from_str("[memory]\nenabled = false").unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(! mem.enabled);
    });
}
#[test]
fn memory_config_env_var_enables() {
    with_kigi_memory(
        "1",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let mem = MemoryConfig::resolve(false, false, &config, None);
            assert!(mem.enabled);
        },
    );
}
#[test]
fn memory_config_env_var_true_enables() {
    with_kigi_memory(
        "true",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let mem = MemoryConfig::resolve(false, false, &config, None);
            assert!(mem.enabled);
        },
    );
}
#[test]
fn memory_config_env_var_zero_does_not_enable() {
    with_kigi_memory(
        "0",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let mem = MemoryConfig::resolve(false, false, &config, None);
            assert!(! mem.enabled, "KIGI_MEMORY=0 should not enable memory");
        },
    );
}
#[test]
fn memory_config_env_var_false_does_not_enable() {
    with_kigi_memory(
        "false",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let mem = MemoryConfig::resolve(false, false, &config, None);
            assert!(! mem.enabled, "KIGI_MEMORY=false should not enable memory");
        },
    );
}
#[test]
fn memory_config_cli_overrides_toml_disabled() {
    without_kigi_memory(|| {
        let config: toml::Value = toml::from_str("[memory]\nenabled = false").unwrap();
        let mem = MemoryConfig::resolve(true, false, &config, None);
        assert!(mem.enabled, "CLI flag should override config file");
    });
}
#[test]
fn memory_config_env_zero_force_disables_toml_enabled() {
    with_kigi_memory(
        "0",
        || {
            let config: toml::Value = toml::from_str("[memory]\nenabled = true")
                .unwrap();
            let mem = MemoryConfig::resolve(false, false, &config, None);
            assert!(
                ! mem.enabled,
                "KIGI_MEMORY=0 should force-disable even when TOML enables memory"
            );
        },
    );
}
#[test]
fn memory_config_env_false_force_disables_toml_enabled() {
    with_kigi_memory(
        "false",
        || {
            let config: toml::Value = toml::from_str("[memory]\nenabled = true")
                .unwrap();
            let mem = MemoryConfig::resolve(false, false, &config, None);
            assert!(
                ! mem.enabled,
                "KIGI_MEMORY=false should force-disable even when TOML enables memory"
            );
        },
    );
}
#[test]
fn memory_config_cli_flag_overrides_env_disable() {
    with_kigi_memory(
        "0",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let mem = MemoryConfig::resolve(true, false, &config, None);
            assert!(
                mem.enabled, "CLI --experimental-memory should override KIGI_MEMORY=0"
            );
        },
    );
}
#[test]
fn memory_config_no_memory_overrides_all() {
    with_kigi_memory(
        "1",
        || {
            let config: toml::Value = toml::from_str("[memory]\nenabled = true")
                .unwrap();
            let mem = MemoryConfig::resolve(true, true, &config, None);
            assert!(
                ! mem.enabled,
                "--no-memory should override --experimental-memory, KIGI_MEMORY=1, and TOML enabled=true"
            );
        },
    );
}
#[test]
fn memory_config_no_memory_alone_disables() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let mem = MemoryConfig::resolve(false, true, &config, None);
        assert!(! mem.enabled, "--no-memory alone should disable");
    });
}
#[test]
fn memory_config_no_memory_overrides_env_enable() {
    with_kigi_memory(
        "1",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let mem = MemoryConfig::resolve(false, true, &config, None);
            assert!(! mem.enabled, "--no-memory should override KIGI_MEMORY=1");
        },
    );
}
#[test]
fn memory_config_no_memory_overrides_toml_enabled() {
    without_kigi_memory(|| {
        let config: toml::Value = toml::from_str("[memory]\nenabled = true").unwrap();
        let mem = MemoryConfig::resolve(false, true, &config, None);
        assert!(! mem.enabled, "--no-memory should override TOML enabled=true");
    });
}
#[test]
fn memory_config_no_memory_overrides_remote_enabled() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            memory_enabled: Some(true),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, true, &config, Some(&remote));
        assert!(! mem.enabled, "--no-memory should override remote memory_enabled=true");
    });
}
#[test]
fn memory_config_defaults_are_correct() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert_eq!(mem.index.max_chunk_chars, 1600);
        assert_eq!(mem.index.chunk_overlap_chars, 320);
        assert_eq!(mem.embedding.provider, "api");
        assert_eq!(mem.embedding.model, None);
        assert_eq!(mem.embedding.dimensions, 1024);
        assert_eq!(mem.search.max_results, 6);
        assert!((mem.search.min_score - 0.35).abs() < f32::EPSILON);
        assert!((mem.search.vector_weight - 0.7).abs() < f32::EPSILON);
        assert!((mem.search.text_weight - 0.3).abs() < f32::EPSILON);
        assert!((mem.search.recency_decay - 0.95).abs() < f32::EPSILON);
        assert!(mem.search.temporal_decay.enabled);
        assert!((mem.search.temporal_decay.half_life_days - 7.0).abs() < f64::EPSILON);
        assert!(! mem.search.mmr.enabled);
        assert!((mem.search.mmr.lambda - 0.7).abs() < f64::EPSILON);
        assert!((mem.search.source_weights["workspace"] - 1.0).abs() < f32::EPSILON);
        assert!((mem.search.source_weights["session"] - 1.0).abs() < f32::EPSILON);
        assert!((mem.search.source_weights["global"] - 1.0).abs() < f32::EPSILON);
        assert!(mem.initial_injection.enabled);
        assert_eq!(mem.initial_injection.min_score, None);
        assert!(mem.session.save_on_end);
        assert!(mem.flush.enabled);
        assert_eq!(mem.flush.soft_threshold_tokens, 4000);
        assert!(mem.flush.flush_model.is_none());
        assert_eq!(mem.flush.max_flush_write_chars, 8000);
        assert!(mem.flush.idle_timeout_secs.is_none());
        assert!(mem.pruning.enabled);
        assert_eq!(mem.pruning.keep_last_n_turns, 3);
        assert_eq!(mem.pruning.soft_trim_threshold, 4000);
        assert_eq!(mem.pruning.soft_trim_head, 1500);
        assert_eq!(mem.pruning.soft_trim_tail, 1500);
        assert_eq!(mem.pruning.hard_clear_age_turns, 10);
        assert!(mem.watcher.enabled);
        assert_eq!(mem.watcher.stale_claim_secs, 60);
        assert!(mem.dream.enabled);
        assert_eq!(mem.dream.min_hours, 4);
        assert_eq!(mem.dream.min_sessions, 3);
        assert_eq!(mem.dream.stale_lock_secs, 3600);
        assert_eq!(mem.dream.check_interval_secs, None);
    });
}
/// A TOML config carrying the legacy `debounce_ms` key must still parse:
/// serde silently ignores fields absent from `MemoryWatcherConfig`.
#[test]
fn memory_config_watcher_debounce_ms_in_toml_is_silently_ignored() {
    without_kigi_memory(|| {
        let toml_str = "[memory.watcher]\nenabled = true\ndebounce_ms = 2000\n";
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.watcher.enabled);
        assert_eq!(mem.watcher.stale_claim_secs, 60);
    });
}
#[test]
fn memory_config_full_toml_parsing() {
    without_kigi_memory(|| {
        let toml_str = r#"
[memory]
enabled = true

[memory.index]
max_chunk_chars = 2000
chunk_overlap_chars = 400

[memory.embedding]
provider = "local"
model = "all-MiniLM-L6-v2"
dimensions = 384

[memory.search]
max_results = 10
min_score = 0.5
vector_weight = 0.6
text_weight = 0.4
recency_decay = 0.9

[memory.initial_injection]
enabled = false
min_score = 0.8

[memory.search.temporal_decay]
enabled = true
half_life_days = 14.0

[memory.search.source_weights]
workspace = 1.0
session = 0.8
global = 0.5

[memory.session]
save_on_end = false

[compaction.memory_flush]
enabled = false
soft_threshold_tokens = 8000
flush_model = "kigi-4"
max_flush_write_chars = 16000
idle_timeout_secs = 300
semantic_dedup_threshold = 0.85

[compaction.pruning]
enabled = false
keep_last_n_turns = 5
soft_trim_threshold = 8000
soft_trim_head = 3000
soft_trim_tail = 3000
hard_clear_age_turns = 20
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.enabled);
        assert_eq!(mem.index.max_chunk_chars, 2000);
        assert_eq!(mem.index.chunk_overlap_chars, 400);
        assert_eq!(mem.embedding.provider, "local");
        assert_eq!(mem.embedding.model.as_deref(), Some("all-MiniLM-L6-v2"));
        assert_eq!(mem.embedding.dimensions, 384);
        assert_eq!(mem.search.max_results, 10);
        assert!((mem.search.min_score - 0.5).abs() < f32::EPSILON);
        assert!(! mem.initial_injection.enabled);
        assert_eq!(mem.initial_injection.min_score, Some(0.8));
        assert!(mem.search.temporal_decay.enabled);
        assert!((mem.search.temporal_decay.half_life_days - 14.0).abs() < f64::EPSILON);
        assert!((mem.search.source_weights["global"] - 0.5).abs() < f32::EPSILON);
        assert!(! mem.session.save_on_end);
        assert!(! mem.flush.enabled);
        assert_eq!(mem.flush.soft_threshold_tokens, 8000);
        assert_eq!(mem.flush.flush_model.as_deref(), Some("kigi-4"));
        assert_eq!(mem.flush.max_flush_write_chars, 16000);
        assert_eq!(mem.flush.idle_timeout_secs, Some(300));
        assert_eq!(mem.flush.semantic_dedup_threshold, Some(0.85));
        assert!(! mem.pruning.enabled);
        assert_eq!(mem.pruning.keep_last_n_turns, 5);
        assert_eq!(mem.pruning.hard_clear_age_turns, 20);
    });
}
#[test]
fn memory_config_partial_toml_uses_defaults_for_missing() {
    without_kigi_memory(|| {
        let toml_str = r#"
[memory]
enabled = true

[memory.index]
max_chunk_chars = 3200
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.enabled);
        assert_eq!(mem.index.max_chunk_chars, 3200);
        assert_eq!(mem.index.chunk_overlap_chars, 320);
        assert_eq!(mem.embedding.dimensions, 1024);
        assert_eq!(mem.search.max_results, 6);
        assert!(mem.flush.enabled);
        assert!(mem.pruning.enabled);
    });
}
#[test]
fn memory_config_remote_settings_enable() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            memory_enabled: Some(true),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(mem.enabled, "remote memory_enabled=true should enable memory");
    });
}
#[test]
fn memory_config_remote_settings_pruning() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            pruning_enabled: Some(true),
            pruning_keep_last_n_turns: Some(5),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(mem.pruning.enabled);
        assert_eq!(mem.pruning.keep_last_n_turns, 5);
    });
}
#[test]
fn memory_config_remote_settings_initial_injection() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            memory_initial_injection_enabled: Some(false),
            memory_initial_injection_min_score: Some(0.77),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(! mem.initial_injection.enabled);
        assert_eq!(mem.initial_injection.min_score, Some(0.77));
    });
}
#[test]
fn memory_config_local_initial_injection_overrides_remote() {
    without_kigi_memory(|| {
        let toml_str = r#"
[memory.initial_injection]
enabled = true
min_score = 0.25
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let remote = crate::util::config::RemoteSettings {
            memory_initial_injection_enabled: Some(false),
            memory_initial_injection_min_score: Some(0.77),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(mem.initial_injection.enabled);
        assert_eq!(mem.initial_injection.min_score, Some(0.25));
    });
}
#[test]
fn memory_config_local_disabled_blocks_remote_enable() {
    without_kigi_memory(|| {
        let config: toml::Value = toml::from_str("[memory]\nenabled = false").unwrap();
        let remote = crate::util::config::RemoteSettings {
            memory_enabled: Some(true),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(
            ! mem.enabled, "local [memory] enabled=false should block remote enable"
        );
    });
}
#[test]
fn memory_config_local_overrides_remote() {
    without_kigi_memory(|| {
        let toml_str = r#"
[memory.search]
max_results = 20
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let remote = crate::util::config::RemoteSettings {
            memory_search_max_results: Some(5),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert_eq!(mem.search.max_results, 20, "local config should override remote");
    });
}
#[test]
fn memory_config_remote_none_is_noop() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let mem_without = MemoryConfig::resolve(false, false, &config, None);
        let mem_with_empty = MemoryConfig::resolve(
            false,
            false,
            &config,
            Some(&crate::util::config::RemoteSettings::default()),
        );
        assert_eq!(mem_without.search.max_results, mem_with_empty.search.max_results);
        assert_eq!(mem_without.enabled, mem_with_empty.enabled);
    });
}
#[test]
fn flush_semantic_dedup_threshold_from_remote_when_no_local_flush() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            flush_semantic_dedup_threshold: Some(0.85),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert_eq!(
            mem.flush.semantic_dedup_threshold, Some(0.85),
            "remote threshold should apply when no local flush config"
        );
    });
}
#[test]
fn flush_semantic_dedup_threshold_clamped_from_remote() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            flush_semantic_dedup_threshold: Some(1.5),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert_eq!(
            mem.flush.semantic_dedup_threshold, Some(1.0),
            "remote threshold above 1.0 should be clamped"
        );
        let remote_neg = crate::util::config::RemoteSettings {
            flush_semantic_dedup_threshold: Some(-0.5),
            ..Default::default()
        };
        let mem_neg = MemoryConfig::resolve(false, false, &config, Some(&remote_neg));
        assert_eq!(
            mem_neg.flush.semantic_dedup_threshold, Some(0.0),
            "remote threshold below 0.0 should be clamped"
        );
    });
}
#[test]
fn flush_semantic_dedup_threshold_local_blocks_remote() {
    without_kigi_memory(|| {
        let toml_str = r#"
[compaction.memory_flush]
enabled = true
semantic_dedup_threshold = 0.88
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let remote = crate::util::config::RemoteSettings {
            flush_semantic_dedup_threshold: Some(0.70),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert_eq!(
            mem.flush.semantic_dedup_threshold, Some(0.88),
            "local flush config should block remote override"
        );
    });
}
#[test]
fn flush_semantic_dedup_threshold_defaults_to_none() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert_eq!(
            mem.flush.semantic_dedup_threshold, None,
            "threshold should default to None (fallback to compiled-in constant)"
        );
    });
}
#[test]
fn memory_dream_config_defaults() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.dream.enabled);
        assert_eq!(mem.dream.min_hours, 4);
        assert_eq!(mem.dream.min_sessions, 3);
        assert_eq!(mem.dream.stale_lock_secs, 3600);
        assert_eq!(mem.dream.check_interval_secs, None);
    });
}
#[test]
fn memory_dream_config_toml_parsing() {
    without_kigi_memory(|| {
        let toml_str = r#"
[memory.dream]
enabled = true
min_hours = 12
min_sessions = 3
stale_lock_secs = 1800
check_interval_secs = 600
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.dream.enabled);
        assert_eq!(mem.dream.min_hours, 12);
        assert_eq!(mem.dream.min_sessions, 3);
        assert_eq!(mem.dream.stale_lock_secs, 1800);
        assert_eq!(mem.dream.check_interval_secs, Some(600));
    });
}
#[test]
fn memory_dream_config_remote_override_when_toml_absent() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            dream_enabled: Some(true),
            dream_min_hours: Some(48),
            dream_min_sessions: Some(10),
            dream_check_interval_secs: Some(900),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(mem.dream.enabled);
        assert_eq!(mem.dream.min_hours, 48);
        assert_eq!(mem.dream.min_sessions, 10);
        assert_eq!(mem.dream.stale_lock_secs, 3600);
        assert_eq!(mem.dream.check_interval_secs, Some(900));
    });
}
#[test]
fn memory_dream_config_remote_ignored_when_toml_present() {
    without_kigi_memory(|| {
        let toml_str = r#"
[memory.dream]
enabled = false
min_hours = 6
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let remote = crate::util::config::RemoteSettings {
            dream_enabled: Some(true),
            dream_min_hours: Some(48),
            dream_min_sessions: Some(10),
            dream_check_interval_secs: Some(300),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(! mem.dream.enabled, "local TOML should win over remote");
        assert_eq!(mem.dream.min_hours, 6);
        assert_eq!(mem.dream.min_sessions, 3);
        assert_eq!(mem.dream.check_interval_secs, None);
    });
}
#[test]
fn expands_multiple_vars_in_one_string() {
    with_env_var(
        "KIGI_TEST_USER",
        "alice",
        || {
            with_env_var(
                "KIGI_TEST_ROOT",
                "/a/b/c/d",
                || {
                    let toml_str = r#"
[mcp_servers.test]
command = "$KIGI_TEST_USER $KIGI_TEST_ROOT"
"#;
                    let mut value = toml::from_str::<toml::Value>(toml_str).unwrap();
                    expand_env_vars_in_toml(&mut value);
                    let toml::Value::Table(table) = value else {
                        panic!("Expected table root");
                    };
                    let Some(toml::Value::Table(mcp_servers)) = table.get("mcp_servers")
                    else {
                        panic!("Expected mcp_servers table");
                    };
                    let Some(toml::Value::Table(test)) = mcp_servers.get("test") else {
                        panic!("Expected test table");
                    };
                    let command = test.get("command").and_then(|v| v.as_str()).unwrap();
                    assert_eq!(command, "alice /a/b/c/d");
                },
            );
        },
    );
}
#[test]
fn effective_half_life_temporal_decay_enabled() {
    let config = MemorySearchConfig {
        temporal_decay: TemporalDecayConfig {
            enabled: true,
            half_life_days: 14.0,
        },
        ..Default::default()
    };
    assert_eq!(config.effective_half_life_days(), Some(14.0));
}
#[test]
fn effective_half_life_temporal_decay_enabled_zero_disables() {
    let config = MemorySearchConfig {
        temporal_decay: TemporalDecayConfig {
            enabled: true,
            half_life_days: 0.0,
        },
        ..Default::default()
    };
    assert_eq!(
        config.effective_half_life_days(), None,
        "zero half_life_days should disable decay"
    );
}
#[test]
fn effective_half_life_temporal_decay_enabled_negative_disables() {
    let config = MemorySearchConfig {
        temporal_decay: TemporalDecayConfig {
            enabled: true,
            half_life_days: -5.0,
        },
        ..Default::default()
    };
    assert_eq!(
        config.effective_half_life_days(), None,
        "negative half_life_days should disable decay"
    );
}
#[test]
fn effective_half_life_disabled_default_recency_returns_none() {
    let config = MemorySearchConfig {
        temporal_decay: TemporalDecayConfig {
            enabled: false,
            half_life_days: 30.0,
        },
        recency_decay: DEFAULT_RECENCY_DECAY,
        ..Default::default()
    };
    assert_eq!(
        config.effective_half_life_days(), None,
        "disabled + default recency_decay should return None"
    );
}
#[test]
fn effective_half_life_disabled_legacy_recency_converts() {
    let config = MemorySearchConfig {
        temporal_decay: TemporalDecayConfig {
            enabled: false,
            half_life_days: 30.0,
        },
        recency_decay: 0.9,
        ..Default::default()
    };
    let half_life = config
        .effective_half_life_days()
        .expect("should convert legacy recency_decay=0.9");
    assert!(
        (half_life - 6.58).abs() < 0.1,
        "recency_decay=0.9 should convert to ~6.58 day half-life, got {half_life}"
    );
}
#[test]
fn effective_half_life_disabled_legacy_recency_098() {
    let config = MemorySearchConfig {
        temporal_decay: TemporalDecayConfig {
            enabled: false,
            half_life_days: 30.0,
        },
        recency_decay: 0.98,
        ..Default::default()
    };
    let half_life = config
        .effective_half_life_days()
        .expect("should convert legacy recency_decay=0.98");
    assert!(
        (half_life - 34.3).abs() < 0.5,
        "recency_decay=0.98 should convert to ~34.3 day half-life, got {half_life}"
    );
}
#[test]
fn effective_half_life_disabled_legacy_recency_out_of_range_ignored() {
    for bad_value in [0.0_f32, 1.0, -0.5, 1.5] {
        let config = MemorySearchConfig {
            temporal_decay: TemporalDecayConfig {
                enabled: false,
                half_life_days: 30.0,
            },
            recency_decay: bad_value,
            ..Default::default()
        };
        assert_eq!(
            config.effective_half_life_days(), None,
            "recency_decay={bad_value} should not convert"
        );
    }
}
#[test]
fn mmr_lambda_clamped_above_one() {
    without_kigi_memory(|| {
        let toml_str = r#"
[memory]
enabled = true

[memory.search.mmr]
enabled = true
lambda = 2.0
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.search.mmr.enabled);
        assert!(
            (mem.search.mmr.lambda - 1.0).abs() < f64::EPSILON,
            "lambda=2.0 should clamp to 1.0, got {}", mem.search.mmr.lambda
        );
    });
}
#[test]
fn mmr_lambda_clamped_below_zero() {
    without_kigi_memory(|| {
        let toml_str = r#"
[memory]
enabled = true

[memory.search.mmr]
enabled = true
lambda = -0.5
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let mem = MemoryConfig::resolve(false, false, &config, None);
        assert!(mem.search.mmr.enabled);
        assert!(
            mem.search.mmr.lambda.abs() < f64::EPSILON,
            "lambda=-0.5 should clamp to 0.0, got {}", mem.search.mmr.lambda
        );
    });
}
#[test]
fn memory_config_remote_temporal_decay() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            memory_temporal_decay_enabled: Some(false),
            memory_temporal_decay_half_life_days: Some(14.0),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(! mem.search.temporal_decay.enabled);
        assert!((mem.search.temporal_decay.half_life_days - 14.0).abs() < f64::EPSILON);
    });
}
#[test]
fn memory_config_remote_mmr() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            memory_mmr_enabled: Some(true),
            memory_mmr_lambda: Some(0.5),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(mem.search.mmr.enabled);
        assert!((mem.search.mmr.lambda - 0.5).abs() < f64::EPSILON);
    });
}
#[test]
fn memory_config_remote_mmr_lambda_clamped() {
    without_kigi_memory(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let remote = crate::util::config::RemoteSettings {
            memory_mmr_lambda: Some(5.0),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(
            (mem.search.mmr.lambda - 1.0).abs() < f64::EPSILON,
            "remote mmr_lambda=5.0 should be clamped to 1.0"
        );
    });
}
#[test]
fn memory_config_local_search_blocks_remote_temporal_decay_and_mmr() {
    without_kigi_memory(|| {
        let toml_str = r#"
[memory.search]
max_results = 8
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let remote = crate::util::config::RemoteSettings {
            memory_temporal_decay_enabled: Some(false),
            memory_mmr_enabled: Some(true),
            memory_mmr_lambda: Some(0.3),
            ..Default::default()
        };
        let mem = MemoryConfig::resolve(false, false, &config, Some(&remote));
        assert!(
            mem.search.temporal_decay.enabled,
            "local search section should block remote temporal_decay override"
        );
        assert!(
            ! mem.search.mmr.enabled,
            "local search section should block remote mmr override"
        );
    });
}
/// Mutex to serialize tests that touch the KIGI_SUBAGENTS env var.
static SUBAGENTS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn without_kigi_subagents<T>(f: impl FnOnce() -> T) -> T {
    let _guard = SUBAGENTS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_var_opt("KIGI_SUBAGENTS", None, f)
}
fn with_kigi_subagents<T>(value: &str, f: impl FnOnce() -> T) -> T {
    let _guard = SUBAGENTS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_var_opt("KIGI_SUBAGENTS", Some(value), f)
}
#[test]
fn subagents_config_default_enabled() {
    without_kigi_subagents(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let sa = SubagentsConfig::resolve(false, &config, None);
        assert!(sa.enabled);
    });
}
#[test]
fn subagents_config_cli_flag_enables() {
    without_kigi_subagents(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let sa = SubagentsConfig::resolve(true, &config, None);
        assert!(sa.enabled);
    });
}
#[test]
fn subagents_config_env_var_enables() {
    with_kigi_subagents(
        "1",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let sa = SubagentsConfig::resolve(false, &config, None);
            assert!(sa.enabled);
        },
    );
}
#[test]
fn subagents_config_env_var_disables() {
    with_kigi_subagents(
        "0",
        || {
            let config: toml::Value = toml::from_str("[subagents]\nenabled = true")
                .unwrap();
            let sa = SubagentsConfig::resolve(false, &config, None);
            assert!(! sa.enabled, "KIGI_SUBAGENTS=0 should override config file");
        },
    );
}
#[test]
fn subagents_config_toml_enables() {
    without_kigi_subagents(|| {
        let config: toml::Value = toml::from_str("[subagents]\nenabled = true").unwrap();
        let sa = SubagentsConfig::resolve(false, &config, None);
        assert!(sa.enabled);
    });
}
#[test]
fn subagents_config_local_disabled_wins() {
    without_kigi_subagents(|| {
        let config: toml::Value = toml::from_str("[subagents]\nenabled = false")
            .unwrap();
        let sa = SubagentsConfig::resolve(false, &config, None);
        assert!(! sa.enabled, "local [subagents] enabled=false should win");
    });
}
#[test]
fn subagents_config_env_var_disables_default() {
    with_kigi_subagents(
        "0",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let sa = SubagentsConfig::resolve(false, &config, None);
            assert!(
                ! sa.enabled, "KIGI_SUBAGENTS=0 should override the enabled default"
            );
        },
    );
}
/// A `subagents_enabled` key served by an old cli-chat-proxy must parse
/// as an unknown key and have no effect on resolution.
#[test]
fn subagents_config_remote_settings_key_is_ignored() {
    without_kigi_subagents(|| {
        let _settings: crate::util::config::RemoteSettings = serde_json::from_str(
                r#"{"subagents_enabled": false}"#,
            )
            .expect("unknown subagents_enabled key must not break parsing");
        let config = toml::Value::Table(toml::map::Map::new());
        let sa = SubagentsConfig::resolve(false, &config, None);
        assert!(sa.enabled);
    });
}
#[test]
fn subagents_config_cli_flag_overrides_env_var() {
    with_kigi_subagents(
        "0",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let sa = SubagentsConfig::resolve(true, &config, None);
            assert!(sa.enabled, "--subagents CLI flag should override KIGI_SUBAGENTS=0");
        },
    );
}
#[test]
fn subagents_config_models_parsed() {
    without_kigi_subagents(|| {
        let config: toml::Value = toml::from_str(
                r#"
                [subagents]
                enabled = true

                [subagents.models]
                explore = "kigi-3-fast"
                plan = "kigi-4.5"
                "#,
            )
            .unwrap();
        let sa = SubagentsConfig::resolve(false, &config, None);
        assert!(sa.enabled);
        assert_eq!(sa.models.len(), 2);
        assert_eq!(sa.models.get("explore").unwrap(), "kigi-3-fast");
        assert_eq!(sa.models.get("plan").unwrap(), "kigi-4.5");
    });
}
#[test]
fn subagents_config_models_empty_when_missing() {
    without_kigi_subagents(|| {
        let config: toml::Value = toml::from_str("[subagents]\nenabled = true").unwrap();
        let sa = SubagentsConfig::resolve(false, &config, None);
        assert!(sa.enabled);
        assert!(sa.models.is_empty());
    });
}
#[test]
fn subagents_config_models_without_enabled() {
    without_kigi_subagents(|| {
        let config: toml::Value = toml::from_str(
                r#"
                [subagents.models]
                explore = "kigi-3-fast"
                "#,
            )
            .unwrap();
        let sa = SubagentsConfig::resolve(false, &config, None);
        assert!(
            ! sa.enabled, "explicit [subagents] section without enabled should be false"
        );
        assert_eq!(sa.models.len(), 1);
        assert_eq!(sa.models.get("explore").unwrap(), "kigi-3-fast");
    });
}
#[test]
fn subagents_config_models_with_env_var_enables() {
    with_kigi_subagents(
        "1",
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [subagents.models]
                explore = "kigi-3-fast"
                "#,
                )
                .unwrap();
            let sa = SubagentsConfig::resolve(false, &config, None);
            assert!(sa.enabled, "KIGI_SUBAGENTS=1 should enable");
            assert_eq!(sa.models.get("explore").unwrap(), "kigi-3-fast");
        },
    );
}
#[test]
fn subagents_config_toggle_mixed_values() {
    without_kigi_subagents(|| {
        let config: toml::Value = toml::from_str(
                r#"
                [subagents]
                enabled = true

                [subagents.toggle]
                explore = true
                plan = false
                general-purpose = true
                code-reviewer = false
                "#,
            )
            .unwrap();
        let sa = SubagentsConfig::resolve(false, &config, None);
        assert!(sa.enabled);
        assert_eq!(sa.toggle.len(), 4);
        assert_eq!(sa.toggle.get("explore").copied(), Some(true));
        assert_eq!(sa.toggle.get("plan").copied(), Some(false));
        assert_eq!(sa.toggle.get("general-purpose").copied(), Some(true));
        assert_eq!(sa.toggle.get("code-reviewer").copied(), Some(false));
    });
}
#[test]
fn subagents_config_toggle_missing_defaults_to_empty() {
    without_kigi_subagents(|| {
        let config: toml::Value = toml::from_str("[subagents]\nenabled = true").unwrap();
        let sa = SubagentsConfig::resolve(false, &config, None);
        assert!(sa.enabled);
        assert!(
            sa.toggle.is_empty(),
            "missing [subagents.toggle] should produce empty HashMap"
        );
    });
}
#[test]
fn subagents_config_is_subagent_enabled_absent_defaults_true() {
    let sa = SubagentsConfig {
        enabled: true,
        toggle: std::collections::HashMap::from([("plan".to_string(), false)]),
        ..Default::default()
    };
    assert!(
        sa.is_subagent_enabled("explore"), "absent key should default to enabled (true)"
    );
    assert!(
        sa.is_subagent_enabled("general-purpose"),
        "absent key should default to enabled (true)"
    );
}
#[test]
fn subagents_config_is_subagent_enabled_false_when_toggled_off() {
    let sa = SubagentsConfig {
        enabled: true,
        toggle: std::collections::HashMap::from([
            ("plan".to_string(), false),
            ("code-reviewer".to_string(), false),
            ("explore".to_string(), true),
        ]),
        ..Default::default()
    };
    assert!(! sa.is_subagent_enabled("plan"), "plan = false should return disabled");
    assert!(
        ! sa.is_subagent_enabled("code-reviewer"),
        "code-reviewer = false should return disabled"
    );
    assert!(sa.is_subagent_enabled("explore"), "explore = true should return enabled");
}
fn with_model_overrides_env_full<T>(
    ss: Option<&str>,
    id: Option<&str>,
    ps: Option<&str>,
    f: impl FnOnce() -> T,
) -> T {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_var_opt(
        "KIGI_SESSION_SUMMARY_MODEL",
        ss,
        || with_env_var_opt(
            "KIGI_IMAGE_DESCRIPTION_MODEL",
            id,
            || with_env_var_opt("KIGI_PROMPT_SUGGESTIONS_MODEL", ps, f),
        ),
    )
}
fn with_model_overrides_env<T>(
    ss: Option<&str>,
    id: Option<&str>,
    f: impl FnOnce() -> T,
) -> T {
    with_model_overrides_env_full(ss, id, None, f)
}
#[test]
fn model_overrides_remote_settings_applies_without_local_config() {
    with_model_overrides_env(
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                session_summary_model: Some("remote-ss".to_owned()),
                image_description_model: Some("remote-id".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, &empty, Some(&remote));
            assert_eq!(cfg.session_summary, Some("remote-ss".to_owned()));
            assert_eq!(cfg.image_description, Some("remote-id".to_owned()));
        },
    );
}
#[test]
fn model_overrides_local_image_description_wins_over_remote() {
    with_model_overrides_env(
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                image_description = "local-id"
                "#,
                )
                .unwrap();
            let remote = crate::util::config::RemoteSettings {
                image_description_model: Some("remote-id".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, &config, Some(&remote));
            assert_eq!(cfg.image_description, Some("local-id".to_owned()));
        },
    );
}
#[test]
fn model_overrides_default_image_description_is_kigi() {
    with_model_overrides_env(
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let cfg = ModelOverrideConfig::resolve(None, &empty, None);
            assert_eq!(
                cfg.image_description, Some(crate
                ::models::default_image_description_model().to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_default_session_summary_is_kigi() {
    with_model_overrides_env(
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let cfg = ModelOverrideConfig::resolve(None, &empty, None);
            assert_eq!(
                cfg.session_summary, Some(crate ::models::default_session_summary_model()
                .to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_local_session_summary_wins_over_remote() {
    with_model_overrides_env(
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                session_summary = "local-ss"
                "#,
                )
                .unwrap();
            let remote = crate::util::config::RemoteSettings {
                session_summary_model: Some("remote-ss".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, &config, Some(&remote));
            assert_eq!(cfg.session_summary, Some("local-ss".to_owned()));
        },
    );
}
#[test]
fn model_overrides_env_session_summary_overrides_remote() {
    with_model_overrides_env(
        Some("env-ss"),
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                session_summary_model: Some("remote-ss".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, &empty, Some(&remote));
            assert_eq!(cfg.session_summary, Some("env-ss".to_owned()));
        },
    );
}
#[test]
fn model_overrides_env_session_summary_overrides_local() {
    with_model_overrides_env(
        Some("env-ss"),
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                session_summary = "local-ss"
                "#,
                )
                .unwrap();
            let cfg = ModelOverrideConfig::resolve(None, &config, None);
            assert_eq!(cfg.session_summary, Some("env-ss".to_owned()));
        },
    );
}
#[test]
fn model_overrides_empty_session_summary_toml_uses_default() {
    with_model_overrides_env(
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                session_summary = ""
                "#,
                )
                .unwrap();
            let cfg = ModelOverrideConfig::resolve(None, &config, None);
            assert_eq!(
                cfg.session_summary, Some(crate ::models::default_session_summary_model()
                .to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_empty_session_summary_remote_uses_default() {
    with_model_overrides_env(
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                session_summary_model: Some("   ".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, &empty, Some(&remote));
            assert_eq!(
                cfg.session_summary, Some(crate ::models::default_session_summary_model()
                .to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_cli_session_summary_overrides_everything() {
    with_model_overrides_env(
        Some("env-ss"),
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                session_summary = "local-ss"
                "#,
                )
                .unwrap();
            let remote = crate::util::config::RemoteSettings {
                session_summary_model: Some("remote-ss".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(
                Some("cli-ss"),
                &config,
                Some(&remote),
            );
            assert_eq!(cfg.session_summary, Some("cli-ss".to_owned()));
        },
    );
}
#[test]
fn model_overrides_empty_cli_session_summary_uses_default() {
    with_model_overrides_env(
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let cfg = ModelOverrideConfig::resolve(Some(""), &empty, None);
            assert_eq!(
                cfg.session_summary, Some(crate ::models::default_session_summary_model()
                .to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_env_image_description_overrides_remote() {
    with_model_overrides_env(
        None,
        Some("env-id"),
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                image_description_model: Some("remote-id".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, &empty, Some(&remote));
            assert_eq!(cfg.image_description, Some("env-id".to_owned()));
        },
    );
}
#[test]
fn model_overrides_env_image_description_overrides_local() {
    with_model_overrides_env(
        None,
        Some("env-id"),
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                image_description = "local-id"
                "#,
                )
                .unwrap();
            let cfg = ModelOverrideConfig::resolve(None, &config, None);
            assert_eq!(cfg.image_description, Some("env-id".to_owned()));
        },
    );
}
#[test]
fn model_overrides_empty_image_description_toml_uses_default() {
    with_model_overrides_env(
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                image_description = ""
                "#,
                )
                .unwrap();
            let cfg = ModelOverrideConfig::resolve(None, &config, None);
            assert_eq!(
                cfg.image_description, Some(crate
                ::models::default_image_description_model().to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_empty_image_description_remote_uses_default() {
    with_model_overrides_env(
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                image_description_model: Some("   ".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, &empty, Some(&remote));
            assert_eq!(
                cfg.image_description, Some(crate
                ::models::default_image_description_model().to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_prompt_suggestion_unpinned_by_default() {
    with_model_overrides_env(
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let cfg = ModelOverrideConfig::resolve(None, &empty, None);
            assert_eq!(cfg.prompt_suggestion, PromptSuggestModelPin::Unpinned);
        },
    );
}
#[test]
fn model_overrides_prompt_suggestion_local_wins_over_remote() {
    with_model_overrides_env(
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                prompt_suggestion = "local-ps"
                "#,
                )
                .unwrap();
            let remote = crate::util::config::RemoteSettings {
                prompt_suggestion_model: Some("remote-ps".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, &config, Some(&remote));
            assert_eq!(
                cfg.prompt_suggestion, PromptSuggestModelPin::Pinned("local-ps"
                .to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_prompt_suggestion_remote_applies_without_local() {
    with_model_overrides_env(
        None,
        None,
        || {
            let empty = toml::Value::Table(toml::map::Map::new());
            let remote = crate::util::config::RemoteSettings {
                prompt_suggestion_model: Some("remote-ps".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, &empty, Some(&remote));
            assert_eq!(
                cfg.prompt_suggestion, PromptSuggestModelPin::Pinned("remote-ps"
                .to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_prompt_suggestion_env_wins_over_local_and_remote() {
    with_model_overrides_env_full(
        None,
        None,
        Some("env-ps"),
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                prompt_suggestion = "local-ps"
                "#,
                )
                .unwrap();
            let remote = crate::util::config::RemoteSettings {
                prompt_suggestion_model: Some("remote-ps".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, &config, Some(&remote));
            assert_eq!(
                cfg.prompt_suggestion, PromptSuggestModelPin::Env("env-ps".to_owned())
            );
        },
    );
}
#[test]
fn model_overrides_prompt_suggestion_blank_values_are_unset() {
    with_model_overrides_env_full(
        None,
        None,
        Some("  "),
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                prompt_suggestion = "local-ps"
                "#,
                )
                .unwrap();
            let cfg = ModelOverrideConfig::resolve(None, &config, None);
            assert_eq!(
                cfg.prompt_suggestion, PromptSuggestModelPin::Pinned("local-ps"
                .to_owned())
            );
        },
    );
    with_model_overrides_env(
        None,
        None,
        || {
            let config: toml::Value = toml::from_str(
                    r#"
                [models]
                prompt_suggestion = "   "
                "#,
                )
                .unwrap();
            let remote = crate::util::config::RemoteSettings {
                prompt_suggestion_model: Some("  ".to_owned()),
                ..Default::default()
            };
            let cfg = ModelOverrideConfig::resolve(None, &config, Some(&remote));
            assert_eq!(cfg.prompt_suggestion, PromptSuggestModelPin::Unpinned);
        },
    );
}
/// Lock shared by every test that touches the env var read by
/// `ToolsConfig::resolve`, so tests can't race.
static TOOLS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn without_kigi_respect_gitignore<T>(f: impl FnOnce() -> T) -> T {
    let _guard = TOOLS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_var_opt("KIGI_RESPECT_GITIGNORE", None, f)
}
fn with_kigi_respect_gitignore<T>(value: &str, f: impl FnOnce() -> T) -> T {
    let _guard = TOOLS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_var_opt("KIGI_RESPECT_GITIGNORE", Some(value), f)
}
#[test]
fn tools_config_default_disabled() {
    without_kigi_respect_gitignore(|| {
        let config = toml::Value::Table(toml::map::Map::new());
        let tc = ToolsConfig::resolve(&config);
        assert!(! tc.respect_gitignore);
    });
}
#[test]
fn tools_config_toml_disables() {
    without_kigi_respect_gitignore(|| {
        let config: toml::Value = toml::from_str("[tools]\nrespect_gitignore = false")
            .unwrap();
        let tc = ToolsConfig::resolve(&config);
        assert!(! tc.respect_gitignore);
    });
}
#[test]
fn tools_config_env_var_disables() {
    with_kigi_respect_gitignore(
        "0",
        || {
            let config = toml::Value::Table(toml::map::Map::new());
            let tc = ToolsConfig::resolve(&config);
            assert!(! tc.respect_gitignore);
        },
    );
}
#[test]
fn tools_config_env_var_overrides_toml() {
    with_kigi_respect_gitignore(
        "1",
        || {
            let config: toml::Value = toml::from_str(
                    "[tools]\nrespect_gitignore = false",
                )
                .unwrap();
            let tc = ToolsConfig::resolve(&config);
            assert!(tc.respect_gitignore, "env var should override config file");
        },
    );
}
#[test]
fn tools_config_env_false_overrides_toml_true() {
    with_kigi_respect_gitignore(
        "false",
        || {
            let config: toml::Value = toml::from_str("[tools]\nrespect_gitignore = true")
                .unwrap();
            let tc = ToolsConfig::resolve(&config);
            assert!(
                ! tc.respect_gitignore,
                "KIGI_RESPECT_GITIGNORE=false should override config file"
            );
        },
    );
}
#[test]
fn roles_parse_from_toml() {
    let toml_str = r#"
            [roles.researcher]
            description = "Deep research agent"
            default_capability_mode = "read-only"
            model = "kigi-3"

            [roles.implementer]
            description = "Implementation agent"
            default_capability_mode = "all"
            prompt_file = ".kigi/prompts/impl.md"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.roles.len(), 2);
    let researcher = cfg.get_role("researcher").unwrap();
    assert_eq!(researcher.description, "Deep research agent");
    assert_eq!(researcher.default_capability_mode.as_deref(), Some("read-only"));
    assert_eq!(researcher.model.as_deref(), Some("kigi-3"));
    assert!(researcher.prompt_file.is_none());
    let implementer = cfg.get_role("implementer").unwrap();
    assert_eq!(implementer.description, "Implementation agent");
    assert_eq!(implementer.default_capability_mode.as_deref(), Some("all"));
    assert!(implementer.model.is_none());
    assert_eq!(implementer.prompt_file.as_deref(), Some(".kigi/prompts/impl.md"));
}
#[test]
fn roles_default_to_empty() {
    let cfg: SubagentsConfig = toml::from_str("").unwrap();
    assert!(cfg.roles.is_empty());
}
#[test]
fn role_lookup_returns_none_for_unknown() {
    let cfg: SubagentsConfig = toml::from_str("").unwrap();
    assert!(cfg.get_role("nonexistent").is_none());
}
#[test]
fn role_minimal_config() {
    let toml_str = r#"
            [roles.simple]
            description = "A simple role"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    let role = cfg.get_role("simple").unwrap();
    assert_eq!(role.description, "A simple role");
    assert!(role.default_capability_mode.is_none());
    assert!(role.model.is_none());
    assert!(role.prompt_file.is_none());
}
#[test]
fn validate_roles_catches_empty_description() {
    let toml_str = r#"
            [roles.bad]
            default_capability_mode = "read-only"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    let errors = cfg.validate_roles();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, "bad");
    assert!(errors[0].1.contains("description is required"));
}
#[test]
fn validate_roles_catches_invalid_capability_mode() {
    let toml_str = r#"
            [roles.bad]
            description = "Has invalid mode"
            default_capability_mode = "readonly"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    let errors = cfg.validate_roles();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].1.contains("invalid default_capability_mode"));
    assert!(errors[0].1.contains("readonly"));
}
#[test]
fn validate_roles_passes_valid_config() {
    let toml_str = r#"
            [roles.good]
            description = "Valid role"
            default_capability_mode = "read-write"
            model = "kigi-3"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    assert!(cfg.validate_roles().is_empty());
}
#[test]
fn validate_roles_catches_empty_prompt_file() {
    let toml_str = r#"
            [roles.bad]
            description = "Has blank prompt_file"
            prompt_file = "  "
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    let errors = cfg.validate_roles();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].1.contains("prompt_file must not be empty"));
}
#[test]
fn validate_roles_accepts_valid_prompt_file() {
    let toml_str = r#"
            [roles.ok]
            description = "Valid prompt file"
            prompt_file = ".kigi/prompts/ok.md"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    assert!(cfg.validate_roles().is_empty());
}
#[test]
fn discover_roles_loads_from_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let roles_dir = tmp.path().join(".kigi").join("roles");
    std::fs::create_dir_all(&roles_dir).unwrap();
    std::fs::write(
            roles_dir.join("reviewer.toml"),
            r#"
                description = "Code reviewer"
                default_capability_mode = "read-only"
            "#,
        )
        .unwrap();
    let mut cfg: SubagentsConfig = toml::from_str("").unwrap();
    cfg.discover_roles(tmp.path());
    let role = cfg.get_role("reviewer").unwrap();
    assert_eq!(role.description, "Code reviewer");
    assert_eq!(role.default_capability_mode.as_deref(), Some("read-only"));
}
#[test]
fn discover_roles_inline_takes_precedence() {
    let tmp = tempfile::TempDir::new().unwrap();
    let roles_dir = tmp.path().join(".kigi").join("roles");
    std::fs::create_dir_all(&roles_dir).unwrap();
    std::fs::write(
            roles_dir.join("researcher.toml"),
            r#"description = "File-based researcher""#,
        )
        .unwrap();
    let mut cfg: SubagentsConfig = toml::from_str(
            r#"
            [roles.researcher]
            description = "Inline researcher"
        "#,
        )
        .unwrap();
    cfg.discover_roles(tmp.path());
    let role = cfg.get_role("researcher").unwrap();
    assert_eq!(
        role.description, "Inline researcher",
        "inline config should take precedence over file"
    );
}
#[test]
fn discover_roles_ignores_non_toml_files() {
    let tmp = tempfile::TempDir::new().unwrap();
    let roles_dir = tmp.path().join(".kigi").join("roles");
    std::fs::create_dir_all(&roles_dir).unwrap();
    std::fs::write(roles_dir.join("readme.md"), "This is not a role definition")
        .unwrap();
    let mut cfg: SubagentsConfig = toml::from_str("").unwrap();
    cfg.discover_roles(tmp.path());
    assert!(cfg.roles.is_empty());
}
#[test]
fn discover_roles_skips_missing_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut cfg: SubagentsConfig = toml::from_str("").unwrap();
    cfg.discover_roles(tmp.path());
    assert!(cfg.roles.is_empty());
}
#[test]
fn personas_parse_from_toml() {
    let toml_str = r#"
            [personas.researcher]
            instructions = "You are a thorough researcher."

            [personas.concise]
            instructions = "Be concise."
            instructions_file = ".kigi/personas/concise.md"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.personas.len(), 2);
    let researcher = cfg.get_persona("researcher").unwrap();
    assert_eq!(
        researcher.instructions.as_deref(), Some("You are a thorough researcher.")
    );
    assert!(researcher.instructions_file.is_none());
    let concise = cfg.get_persona("concise").unwrap();
    assert_eq!(concise.instructions.as_deref(), Some("Be concise."));
    assert_eq!(concise.instructions_file.as_deref(), Some(".kigi/personas/concise.md"));
}
#[test]
fn personas_default_to_empty() {
    let cfg: SubagentsConfig = toml::from_str("").unwrap();
    assert!(cfg.personas.is_empty());
}
#[test]
fn persona_lookup_returns_none_for_unknown() {
    let cfg: SubagentsConfig = toml::from_str("").unwrap();
    assert!(cfg.get_persona("nonexistent").is_none());
}
#[test]
fn discover_personas_loads_from_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join(".kigi").join("personas");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join("friendly.toml"),
            r#"instructions = "Be friendly and warm.""#,
        )
        .unwrap();
    let mut cfg: SubagentsConfig = toml::from_str("").unwrap();
    cfg.discover_personas(tmp.path());
    let p = cfg.get_persona("friendly").unwrap();
    assert_eq!(p.instructions.as_deref(), Some("Be friendly and warm."));
}
#[test]
fn discover_personas_inline_takes_precedence() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join(".kigi").join("personas");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("strict.toml"), r#"instructions = "File-based strict""#)
        .unwrap();
    let mut cfg: SubagentsConfig = toml::from_str(
            r#"
            [personas.strict]
            instructions = "Inline strict"
        "#,
        )
        .unwrap();
    cfg.discover_personas(tmp.path());
    assert_eq!(
        cfg.get_persona("strict").unwrap().instructions.as_deref(),
        Some("Inline strict"),
    );
}
#[test]
fn bundled_personas_and_roles_have_lowest_priority_in_resolve_order() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let workspace = tmp.path().join("workspace");
    let bundled = home.join(".kigi").join("bundled");
    std::fs::create_dir_all(workspace.join(".kigi").join("roles")).unwrap();
    std::fs::create_dir_all(workspace.join(".kigi").join("personas")).unwrap();
    std::fs::create_dir_all(home.join(".kigi").join("roles")).unwrap();
    std::fs::create_dir_all(home.join(".kigi").join("personas")).unwrap();
    std::fs::create_dir_all(bundled.join("roles")).unwrap();
    std::fs::create_dir_all(bundled.join("personas")).unwrap();
    std::fs::write(
            bundled.join("roles/reviewer.toml"),
            r#"description = "Bundled reviewer""#,
        )
        .unwrap();
    std::fs::write(
            bundled.join("personas/reviewer.toml"),
            r#"instructions = "Bundled persona""#,
        )
        .unwrap();
    std::fs::write(
            home.join(".kigi/roles/reviewer.toml"),
            r#"description = "User reviewer""#,
        )
        .unwrap();
    std::fs::write(
            home.join(".kigi/personas/reviewer.toml"),
            r#"instructions = "User persona""#,
        )
        .unwrap();
    std::fs::write(
            workspace.join(".kigi/roles/reviewer.toml"),
            r#"description = "Project reviewer""#,
        )
        .unwrap();
    std::fs::write(
            workspace.join(".kigi/personas/reviewer.toml"),
            r#"instructions = "Project persona""#,
        )
        .unwrap();
    with_env_var(
        "HOME",
        home.to_str().unwrap(),
        || {
            let config = toml::from_str::<
                toml::Value,
            >(
                    r#"
                [subagents]
                enabled = true

                [subagents.roles.reviewer]
                description = "Inline reviewer"

                [subagents.personas.reviewer]
                instructions = "Inline persona"
                "#,
                )
                .unwrap();
            let resolved = SubagentsConfig::resolve(true, &config, Some(&workspace));
            assert_eq!(
                resolved.get_role("reviewer").unwrap().description, "Inline reviewer"
            );
            assert_eq!(
                resolved.get_persona("reviewer").unwrap().instructions.as_deref(),
                Some("Inline persona")
            );
        },
    );
    std::fs::remove_file(workspace.join(".kigi/roles/reviewer.toml")).unwrap();
    std::fs::remove_file(workspace.join(".kigi/personas/reviewer.toml")).unwrap();
    with_env_var(
        "HOME",
        home.to_str().unwrap(),
        || {
            let config = toml::from_str::<
                toml::Value,
            >(
                    r#"
                [subagents]
                enabled = true
                "#,
                )
                .unwrap();
            let resolved = SubagentsConfig::resolve(true, &config, Some(&workspace));
            assert_eq!(
                resolved.get_role("reviewer").unwrap().description, "User reviewer"
            );
            assert_eq!(
                resolved.get_persona("reviewer").unwrap().instructions.as_deref(),
                Some("User persona")
            );
        },
    );
    std::fs::remove_file(home.join(".kigi/roles/reviewer.toml")).unwrap();
    std::fs::remove_file(home.join(".kigi/personas/reviewer.toml")).unwrap();
    with_env_var(
        "HOME",
        home.to_str().unwrap(),
        || {
            let config = toml::from_str::<
                toml::Value,
            >(
                    r#"
                [subagents]
                enabled = true
                "#,
                )
                .unwrap();
            let resolved = SubagentsConfig::resolve(true, &config, Some(&workspace));
            assert_eq!(
                resolved.get_role("reviewer").unwrap().description, "Bundled reviewer"
            );
            assert_eq!(
                resolved.get_persona("reviewer").unwrap().instructions.as_deref(),
                Some("Bundled persona")
            );
        },
    );
}
#[test]
fn render_io_summary_shows_bundled_for_bundled_personas() {
    let persona = SubagentPersona {
        instructions: Some("Bundled instructions".to_string()),
        source_path: Some("/tmp/home/.kigi/bundled/personas/reviewer.toml".to_string()),
        ..Default::default()
    };
    let summary = persona.render_io_summary("reviewer");
    assert!(summary.contains("[bundled]"));
}
#[test]
fn roles_coexist_with_models_and_toggle() {
    let toml_str = r#"
            enabled = true
            [models]
            explore = "kigi-fast"
            [toggle]
            plan = false
            [roles.researcher]
            description = "Research agent"
            default_capability_mode = "read-only"
        "#;
    let cfg: SubagentsConfig = toml::from_str(toml_str).unwrap();
    assert!(cfg.enabled);
    assert_eq!(cfg.models.get("explore").map(| s | s.as_str()), Some("kigi-fast"));
    assert!(! cfg.is_subagent_enabled("plan"));
    assert!(cfg.get_role("researcher").is_some());
}
#[test]
fn add_hooks_path_appends() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let _ = add_hooks_path_to_file("/some/path", &paths_file);
    let content = std::fs::read_to_string(&paths_file).unwrap_or_default();
    assert!(content.contains("/some/path"));
}
#[test]
fn add_hooks_path_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let _ = add_hooks_path_to_file("/dup/path", &paths_file);
    let _ = add_hooks_path_to_file("/dup/path", &paths_file);
    let content = std::fs::read_to_string(&paths_file).unwrap_or_default();
    let count = content.lines().filter(|l| l.trim() == "/dup/path").count();
    assert_eq!(count, 1);
}
#[test]
fn remove_hooks_path_removes() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let _ = add_hooks_path_to_file("/to/remove", &paths_file);
    let _ = remove_hooks_path_from_file("/to/remove", &paths_file);
    let content = std::fs::read_to_string(&paths_file).unwrap_or_default();
    assert!(! content.contains("/to/remove"));
}
#[test]
fn remove_hooks_path_is_noop_if_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let result = remove_hooks_path_from_file("/nonexistent/path", &paths_file);
    assert!(result.is_ok());
}
#[test]
fn remove_hooks_path_preserves_others() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let _ = add_hooks_path_to_file("/keep/me", &paths_file);
    let _ = add_hooks_path_to_file("/remove/me", &paths_file);
    let _ = add_hooks_path_to_file("/keep/me/too", &paths_file);
    let _ = remove_hooks_path_from_file("/remove/me", &paths_file);
    let content = std::fs::read_to_string(&paths_file).unwrap_or_default();
    assert!(content.contains("/keep/me"));
    assert!(content.contains("/keep/me/too"));
    assert!(! content.contains("/remove/me"));
}
#[test]
fn add_hooks_path_succeeds_on_first_add() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let result = add_hooks_path_to_file("/first", &paths_file);
    assert!(result.is_ok());
}
#[test]
fn add_hooks_path_succeeds_on_duplicate() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let _ = add_hooks_path_to_file("/dup", &paths_file);
    let result = add_hooks_path_to_file("/dup", &paths_file);
    assert!(result.is_ok());
}
#[test]
fn remove_hooks_path_succeeds_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let _ = add_hooks_path_to_file("/present", &paths_file);
    let result = remove_hooks_path_from_file("/present", &paths_file);
    assert!(result.is_ok());
}
#[test]
fn remove_hooks_path_succeeds_when_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let paths_file = tmp.path().join("hooks-paths");
    let result = remove_hooks_path_from_file("/missing", &paths_file);
    assert!(result.is_ok());
}
#[test]
fn add_dismissed_plugin_cta_creates_table() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    add_dismissed_plugin_cta_to_file("figma", &config_path).unwrap();
    let content = std::fs::read_to_string(&config_path).unwrap();
    assert!(content.contains("[plugin_cta]"));
    assert!(content.contains("figma"));
    assert!(dismissed_plugin_ctas_in_file(& config_path).contains("figma"));
}
#[test]
fn add_dismissed_plugin_cta_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    add_dismissed_plugin_cta_to_file("notion", &config_path).unwrap();
    add_dismissed_plugin_cta_to_file("notion", &config_path).unwrap();
    let config: toml::Value = toml::from_str(
            &std::fs::read_to_string(&config_path).unwrap(),
        )
        .unwrap();
    let dismissed = config
        .get("plugin_cta")
        .and_then(|v| v.get("dismissed"))
        .and_then(|v| v.as_array())
        .unwrap();
    let count = dismissed.iter().filter(|v| v.as_str() == Some("notion")).count();
    assert_eq!(count, 1);
}
#[test]
fn dismissed_plugin_ctas_reflects_added_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    assert!(! dismissed_plugin_ctas_in_file(& config_path).contains("figma"));
    add_dismissed_plugin_cta_to_file("figma", &config_path).unwrap();
    let dismissed = dismissed_plugin_ctas_in_file(&config_path);
    assert!(dismissed.contains("figma"));
    assert!(! dismissed.contains("notion"));
}
#[test]
fn add_dismissed_plugin_cta_preserves_other_config() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    std::fs::write(&config_path, "[plugins]\ndisabled = [\"keep-me\"]\n").unwrap();
    add_dismissed_plugin_cta_to_file("figma", &config_path).unwrap();
    let config: toml::Value = toml::from_str(
            &std::fs::read_to_string(&config_path).unwrap(),
        )
        .unwrap();
    assert_eq!(
        config.get("plugins").and_then(| v | v.get("disabled")).and_then(| v | v
        .as_array()).and_then(| a | a.first()).and_then(| v | v.as_str()),
        Some("keep-me"),
    );
    assert!(dismissed_plugin_ctas_in_file(& config_path).contains("figma"));
}
#[test]
fn config_layers_user_overrides_managed() {
    let layers = ConfigLayers {
        system_managed: toml::Value::Table(Default::default()),
        managed: toml::from_str("[features]\nfeedback = false\n").unwrap(),
        user: toml::from_str("[features]\nfeedback = true\n").unwrap(),
        user_requirements: None,
        system_requirements: None,
        mdm_requirements: None,
        ..Default::default()
    };
    let cfg = crate::agent::config::Config::new_from_toml_cfg(
            &layers.effective_config_disk_only(),
        )
        .unwrap();
    assert_eq!(Some(true), cfg.features.feedback);
}
/// REGRESSION: the real enterprise two-file merge —
/// `managed_config.toml` (proxy + BYO model host) layered with
/// `requirements.toml` (deployment key + S3 trace upload) via the actual
/// `ConfigLayers::effective_config()` path — must resolve the deployment-config
/// fetch to cli-chat-proxy, never the model host, and must preserve the
/// customer's S3 trace-upload endpoint.
#[test]
#[serial_test::serial]
fn enterprise_two_file_merge_routes_deployment_key_to_proxy() {
    for k in [
        "KIGI_MANAGED_CONFIG_URL",
        "KIGI_CODE_BASE_URL",
        "KIGI_TRACE_UPLOAD_ENDPOINT_URL",
    ] {
        unsafe { std::env::remove_var(k) };
    }
    let managed = toml::from_str(
            r#"
[endpoints]
api_base_url = "https://inference.acme-corp.example/xai/v1"
coding_api_base_url = "https://cli-chat-proxy.kigi.com/v1"

[model.kigi-build]
base_url = "https://inference.acme-corp.example/xai/v1"
env_key = "ANTHROPIC_AUTH_TOKEN"
model = "kigi-4.5"

[models]
default = "kigi-4.5"
"#,
        )
        .unwrap();
    let requirements = toml::from_str(
            r#"
[features]
feedback = true
telemetry = false

[endpoints]
deployment_key = "xai-token-ENTERPRISE"
api_base_url = "https://inference.acme-corp.example/xai/v1"
"#,
        )
        .unwrap();
    let layers = ConfigLayers {
        system_managed: toml::Value::Table(Default::default()),
        managed,
        user: toml::Value::Table(Default::default()),
        user_requirements: Some(requirements),
        system_requirements: None,
        mdm_requirements: None,
        ..Default::default()
    };
    let cfg = crate::agent::config::Config::new_from_toml_cfg(
            &layers.effective_config_disk_only(),
        )
        .unwrap();
    assert_eq!(
        cfg.endpoints.resolve_managed_config_url(),
        "https://cli-chat-proxy.kigi.com/v1/deployment/config"
    );
    assert!(! cfg.endpoints.resolve_managed_config_url().contains("acme-corp"));
    assert!(cfg.endpoints.deployment_key.is_some());
}
#[test]
fn config_layers_origins_tracks_source() {
    use crate::agent::config::ConfigSource;
    let layers = ConfigLayers {
        system_managed: toml::Value::Table(Default::default()),
        managed: toml::from_str("[features]\nfeedback = false\n").unwrap(),
        user: toml::from_str("[ui]\ntheme = \"dark\"\n").unwrap(),
        user_requirements: None,
        system_requirements: None,
        mdm_requirements: None,
        ..Default::default()
    };
    let origins = config_origins(&layers);
    assert_eq!(origins["features.feedback"], ConfigSource::ManagedConfig);
    assert_eq!(origins["ui.theme"], ConfigSource::UserConfig);
}
#[test]
fn config_layers_origins_user_wins() {
    use crate::agent::config::ConfigSource;
    let layers = ConfigLayers {
        system_managed: toml::Value::Table(Default::default()),
        managed: toml::from_str("[features]\nfeedback = false\n").unwrap(),
        user: toml::from_str("[features]\nfeedback = true\n").unwrap(),
        user_requirements: None,
        system_requirements: None,
        mdm_requirements: None,
        ..Default::default()
    };
    let origins = config_origins(&layers);
    assert_eq!(origins["features.feedback"], ConfigSource::UserConfig);
}
#[test]
fn config_layers_system_managed_lowest_priority() {
    let layers = ConfigLayers {
        system_managed: toml::from_str("[features]\nfeedback = false\n").unwrap(),
        managed: toml::Value::Table(Default::default()),
        user: toml::from_str("[features]\nfeedback = true\n").unwrap(),
        user_requirements: None,
        system_requirements: None,
        mdm_requirements: None,
        ..Default::default()
    };
    let cfg = crate::agent::config::Config::new_from_toml_cfg(
            &layers.effective_config_disk_only(),
        )
        .unwrap();
    assert_eq!(Some(true), cfg.features.feedback);
}
#[test]
fn apply_requirements_value_overrides_user_settings() {
    let raw_config: toml::Value = toml::from_str(
            "[cli]\nauto_update = true\nchannel = \"beta\"\n\n[features]\nfeedback = true\nlsp_tools = true\nweb_fetch = true\nwrite_file = true\n\n[ui]\nyolo = true\n\n[models]\ndefault = \"user-model\"\n\n[endpoints]\ncoding_api_base_url = \"https://user-proxy.example/v1\"\napi_base_url = \"https://user-api.example/v1\"\nmodels_base_url = \"https://user-models.example/v1\"\nmodels_list_url = \"https://user-models.example/v1/models\"\n",
        )
        .unwrap();
    let mut cfg = crate::agent::config::Config::new_from_toml_cfg(&raw_config).unwrap();
    cfg.default_yolo_mode = true;
    let requirements: toml::Value = toml::from_str(
            "[cli]\nauto_update = false\nchannel = \"stable\"\n\n[features]\nfeedback = false\nlsp_tools = false\nweb_fetch = false\nwrite_file = false\nremote_fetch = false\n\n[ui]\nyolo = false\n\n[models]\ndefault = \"managed-model\"\n\n[endpoints]\ncoding_api_base_url = \"https://managed-proxy.example/v1\"\napi_base_url = \"https://managed-api.example/v1\"\nmodels_base_url = \"https://managed-models.example/v1\"\nmodels_list_url = \"https://managed-models.example/v1/models\"\ndeployment_key = \"enterprise-deploy-key-should-not-log\"\n",
        )
        .unwrap();
    let source = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/test/requirements.toml"),
    };
    let enforced = apply_requirements_inner(&mut cfg, &requirements, &source);
    assert_eq!(Some(false), cfg.features.feedback);
    assert_eq!(Some(false), cfg.features.lsp_tools);
    assert_eq!(Some(false), cfg.features.web_fetch);
    assert_eq!(Some(false), cfg.features.write_file);
    assert_eq!(Some(false), cfg.requirements.remote_fetch.pinned());
    assert!(
        enforced.iter().any(| e | e.path == "features.remote_fetch" && e.value ==
        "false")
    );
    assert_eq!(Some(false), cfg.cli.auto_update);
    assert!(! cfg.ui.yolo);
    assert!(! cfg.default_yolo_mode);
    assert_eq!(Some("managed-model"), cfg.models.default.as_deref());
    assert_eq!(Some("stable"), cfg.cli.channel.as_deref());
    assert_eq!(
        Some("https://managed-proxy.example/v1"), cfg.endpoints.coding_api_base_url
        .as_deref()
    );
    assert_eq!(
        Some("https://managed-api.example/v1"),
        cfg.endpoints.api_base_url.as_deref()
    );
    assert_eq!(
        Some("https://managed-models.example/v1"), cfg.endpoints.models_base_url
        .as_deref()
    );
    assert_eq!(
        Some("https://managed-models.example/v1/models"), cfg.endpoints.models_list_url
        .as_deref()
    );
    assert!(
        enforced.iter().any(| e | e.path == "ui.yolo" && e.value == "--yolo blocked")
    );
    assert_eq!(
        Some("enterprise-deploy-key-should-not-log"), cfg.endpoints.deployment_key
        .as_deref()
    );
    assert!(
        enforced.iter().any(| e | e.path == "endpoints.deployment_key" && e.value ==
        "[redacted]"), "deployment_key must use the redacted enforce_str variant"
    );
    assert!(
        enforced.iter().all(| e | e.path != "endpoints.deployment_key" || e.value !=
        "enterprise-deploy-key-should-not-log"),
        "raw deployment_key must not appear in enforced audit entries"
    );
}
/// Strict precedence: requirement always wins (covers from-None and
/// from-higher-user cases). The enforced floor lives in
/// `resolve_minimum_version`, not this field.
#[test]
fn apply_requirements_pins_minimum_version() {
    let source = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/test/requirements.toml"),
    };
    let req: toml::Value = toml::from_str("[cli]\nminimum_version = \"0.1.150\"\n")
        .unwrap();
    let mut cfg_a = crate::agent::config::Config::new_from_toml_cfg(
            &toml::from_str::<toml::Value>("").unwrap(),
        )
        .unwrap();
    apply_requirements_inner(&mut cfg_a, &req, &source);
    assert_eq!(cfg_a.cli.minimum_version.as_deref(), Some("0.1.150"));
    let mut cfg_b = crate::agent::config::Config::new_from_toml_cfg(
            &toml::from_str::<toml::Value>("[cli]\nminimum_version = \"0.1.200\"\n")
                .unwrap(),
        )
        .unwrap();
    apply_requirements_inner(&mut cfg_b, &req, &source);
    assert_eq!(cfg_b.cli.minimum_version.as_deref(), Some("0.1.150"));
}
#[test]
fn apply_requirements_pins_voice_mode_false() {
    let mut cfg = crate::agent::config::Config::default();
    let req: toml::Value = toml::from_str("[features]\nvoice_mode = false\n").unwrap();
    let source = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/test/requirements.toml"),
    };
    apply_requirements_inner(&mut cfg, &req, &source);
    assert_eq!(cfg.requirements.voice_mode.pinned(), Some(false));
    assert_eq!(cfg.features.voice_mode, Some(false));
    assert!(! cfg.resolve_voice_mode().value);
}
/// Requirements enforcement beats a campaign-supplied default. The on-disk
/// `Config` arrives campaign-overlaid (`models.default` = a campaign value);
/// a requirements layer enforcing `[models] default` clamps it back.
#[test]
fn apply_requirements_default_beats_campaign_default() {
    let raw: toml::Value = toml::from_str("[models]\ndefault = \"campaign-model\"\n")
        .unwrap();
    let mut cfg = crate::agent::config::Config::new_from_toml_cfg(&raw).unwrap();
    assert_eq!(
        cfg.models.default.as_deref(), Some("campaign-model"),
        "precondition: config carries the campaign default"
    );
    let req: toml::Value = toml::from_str("[models]\ndefault = \"enforced-model\"\n")
        .unwrap();
    let source = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/test/requirements.toml"),
    };
    let enforced = apply_requirements_inner(&mut cfg, &req, &source);
    assert_eq!(
        cfg.models.default.as_deref(), Some("enforced-model"),
        "requirements default must beat the campaign default"
    );
    assert!(
        enforced.iter().any(| e | e.path == "models.default" && e.value ==
        "enforced-model"), "the enforcement must be reported in the audit trail"
    );
}
#[test]
fn validate_hooks_path_rejects_relative_path() {
    let result = validate_hooks_path("relative/path/hooks");
    assert!(result.is_err());
    assert!(
        result.unwrap_err().to_string().contains("absolute"), "should mention 'absolute'"
    );
}
#[test]
fn validate_hooks_path_rejects_outside_kigi_home() {
    let result = validate_hooks_path("/tmp/evil-hooks");
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("must be under ~/.kigi/"),
        "should mention ~/.kigi/ restriction, got: {msg}"
    );
}
#[test]
fn validate_hooks_path_rejects_traversal_attack() {
    let kigi_home = crate::util::kigi_home::kigi_home();
    let traversal = format!("{}/../evil", kigi_home.display());
    let result = validate_hooks_path(&traversal);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("must be under ~/.kigi/"),
        "traversal should be rejected, got: {msg}"
    );
}
#[test]
fn validate_hooks_path_accepts_kigi_hooks_subdir() {
    let kigi_home = crate::util::kigi_home::kigi_home();
    let valid_path = kigi_home.join("hooks").join("my-hooks");
    let _ = std::fs::create_dir_all(&valid_path);
    let result = validate_hooks_path(valid_path.to_str().unwrap());
    assert!(result.is_ok(), "path under ~/.kigi/ should be accepted");
}
#[test]
fn managed_settings_disables_features_and_requirements_overrides() {
    use kigi_workspace::permission::resolution::ManagedSettingsFeatures;
    let mut cfg = crate::agent::config::Config::default();
    cfg.features.feedback = Some(true);
    cfg.default_yolo_mode = true;
    let features = ManagedSettingsFeatures {
        disable_telemetry: Some(true),
        disable_feedback: Some(true),
        disable_yolo: Some(true),
        source_path: Some(std::path::PathBuf::from("/etc/managed-settings.json")),
    };
    let enforced = apply_managed_settings_features_inner(&mut cfg, &features);
    assert_eq!(cfg.features.feedback, Some(false));
    assert!(cfg.default_yolo_mode);
    assert_eq!(enforced.len(), 1);
    assert!(! enforced.iter().any(| e | e.path == "ui.yolo"));
    let req: toml::Value = toml::from_str(
            "[features]\nfeedback = true\n\n[ui]\nyolo = true\n",
        )
        .unwrap();
    let source = RequirementSource::Requirements {
        path: std::path::PathBuf::from("/test/requirements.toml"),
    };
    apply_requirements_inner(&mut cfg, &req, &source);
    assert_eq!(cfg.features.feedback, Some(true));
    assert!(cfg.ui.yolo);
}
/// REGRESSION: external managed-settings.json is advisory, not authoritative.
/// disableBypassPermissionsMode (-> features.disable_yolo) must NOT clamp the user's own kigi yolo.
#[test]
fn managed_settings_does_not_override_user_yolo() {
    use kigi_workspace::permission::resolution::ManagedSettingsFeatures;
    let mut cfg = crate::agent::config::Config::default();
    cfg.features.feedback = Some(true);
    cfg.ui.yolo = true;
    cfg.default_yolo_mode = true;
    let features = ManagedSettingsFeatures {
        disable_telemetry: Some(true),
        disable_feedback: Some(true),
        disable_yolo: Some(true),
        source_path: Some(
            std::path::PathBuf::from("/etc/claude-code/managed-settings.json"),
        ),
    };
    let enforced = apply_managed_settings_features_inner(&mut cfg, &features);
    assert_eq!(cfg.features.feedback, Some(false));
    assert!(cfg.ui.yolo);
    assert!(cfg.default_yolo_mode);
    assert_eq!(enforced.len(), 1);
    assert!(! enforced.iter().any(| e | e.path == "ui.yolo"));
}
/// Simulate a release-stamped build so the folder-trust gate engages (a
/// local/dev build auto-trusts). Hold the returned guard for the test body.
fn simulate_release_build() -> kigi_test_support::EnvGuard {
    kigi_test_support::EnvGuard::set(kigi_version::TEST_VERSION_ENV, "0.0.0-sim")
}
/// SECURITY (plugin-RCE): a PROJECT-declared `[plugins].paths` loads as an
/// auto-enabled, auto-trusted ConfigPath plugin, so it must merge into the
/// effective config ONLY when the folder is trusted; project
/// `[plugins].disabled` is never gated. The closing set-difference proves
/// the gate toggles ONLY that path (user/global paths pass through both
/// verdicts untouched). KIGI_SHARE_DIR-isolated + `#[serial]` for folder-trust
/// store hygiene (empty store ⇒ deterministic untrusted;
/// `EnvGuard` restores KIGI_SHARE_DIR even on panic). No user-global
/// `$KIGI_SHARE_DIR/config.toml` is seeded: `kigi_home()` is `OnceLock`-cached,
/// so under a shared-process harness (Bazel) such a seed is read
/// non-deterministically — reliable only under nextest's process-per-test
/// isolation.
#[test]
#[serial_test::serial]
fn resolve_effective_plugins_config_gates_project_paths_on_folder_trust() {
    use kigi_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("KIGI_SHARE_DIR", home.path());
    let _flag = EnvGuard::unset("KIGI_FOLDER_TRUST");
    let _sim = simulate_release_build();
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    let kigi = repo.path().join(".kigi");
    std::fs::create_dir_all(&kigi).unwrap();
    std::fs::write(
            kigi.join("config.toml"),
            "[plugins]\npaths = [\"./proj-plugin\"]\ndisabled = [\"proj-bad\"]\n",
        )
        .unwrap();
    let cwd = repo.path();
    let proj_path = "./proj-plugin".to_string();
    let proj_disabled = "proj-bad".to_string();
    let untrusted = resolve_effective_plugins_config(cwd);
    assert!(
        ! untrusted.paths.contains(& proj_path),
        "untrusted folder must NOT merge the project [plugins].paths"
    );
    assert!(
        untrusted.disabled.contains(& proj_disabled),
        "project [plugins].disabled must merge even when untrusted (fail-safe)"
    );
    crate::agent::folder_trust::grant_folder_trust(cwd);
    let trusted = resolve_effective_plugins_config(cwd);
    assert!(
        trusted.paths.contains(& proj_path),
        "trusted folder must merge the project [plugins].paths"
    );
    assert!(
        trusted.disabled.contains(& proj_disabled),
        "project [plugins].disabled must merge when trusted too"
    );
    let trusted_minus_project: Vec<String> = trusted
        .paths
        .iter()
        .filter(|p| *p != &proj_path)
        .cloned()
        .collect();
    assert_eq!(
        trusted_minus_project, untrusted.paths,
        "the trust gate must toggle ONLY the project path; user/global paths unaffected"
    );
}
/// SECURITY (plugin-RCE) end-to-end: prove through the REAL `discover_plugins`
/// that a PROJECT-declared `[plugins].paths` ConfigPath plugin is EXCLUDED
/// from discovery while untrusted and included once trusted. The Part-2
/// set-difference test covers the config merge; this closes the loop at the
/// discovery boundary (if it is never discovered it can never activate).
/// Mirrors the Project-scope analog `discover_real_project_plugin_gated_on_project_trusted`
/// in `kigi-agent`. An ABSOLUTE plugin path is used so the merged
/// `config_paths` entry resolves against the repo — `discover_plugins`' `is_dir()`
/// check resolves a relative `./x` against the process cwd, not `cwd`.
/// KIGI_SHARE_DIR-isolated + `#[serial]` (`EnvGuard` restores it even on panic).
#[test]
#[serial_test::serial]
fn discover_plugins_excludes_untrusted_configpath_plugin_end_to_end() {
    use kigi_agent::plugins::{TrustStore, discover_plugins};
    use kigi_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("KIGI_SHARE_DIR", home.path());
    let _flag = EnvGuard::unset("KIGI_FOLDER_TRUST");
    let _sim = simulate_release_build();
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    let cwd = repo.path();
    let plugin_dir = cwd.join("cfgpath-probe");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("plugin.json"), r#"{"name":"cfgpath-probe"}"#)
        .unwrap();
    let kigi = cwd.join(".kigi");
    std::fs::create_dir_all(&kigi).unwrap();
    std::fs::write(
            kigi.join("config.toml"),
            format!("[plugins]\npaths = ['{}']\n", plugin_dir.display()),
        )
        .unwrap();
    let trust_store = TrustStore::load_from(home.path().join("plugin-trust"));
    let untrusted_dc = resolve_effective_plugins_config(cwd).to_discovery_config();
    let untrusted_verdict = crate::agent::folder_trust::project_scope_allowed(cwd);
    assert!(
        ! untrusted_verdict,
        "a fresh repo declaring [plugins].paths must resolve untrusted"
    );
    assert!(
        ! untrusted_dc.config_paths.iter().any(| p | p.ends_with("cfgpath-probe")),
        "untrusted: the project path must be absent from config_paths"
    );
    let untrusted_found = discover_plugins(
            Some(cwd),
            &untrusted_dc,
            &trust_store,
            untrusted_verdict,
        )
        .iter()
        .any(|p| p.manifest.name == "cfgpath-probe");
    assert!(
        ! untrusted_found,
        "untrusted folder must EXCLUDE the ConfigPath plugin from discovery"
    );
    crate::agent::folder_trust::grant_folder_trust(cwd);
    crate::agent::folder_trust::resolve_and_record(cwd, None, false);
    let trusted_dc = resolve_effective_plugins_config(cwd).to_discovery_config();
    let trusted_verdict = crate::agent::folder_trust::project_scope_allowed(cwd);
    assert!(trusted_verdict, "a store-granted repo must resolve trusted");
    let trusted_found = discover_plugins(
            Some(cwd),
            &trusted_dc,
            &trust_store,
            trusted_verdict,
        )
        .iter()
        .any(|p| p.manifest.name == "cfgpath-probe");
    assert!(trusted_found, "trusted folder must DISCOVER the merged ConfigPath plugin");
}
/// Kill-switch ordering regression: `resolve_effective_plugins_config` reads
/// the folder-trust gate internally, so its call sites (commands/list, plugin
/// fan-out, reload) resolve with the REAL RemoteSettings first. A cold key
/// under an org kill-switch must end up allowed — if the plugins-config read
/// ran first, the gate's remote-less backstop would record a durable
/// kill-switch-blind deny that `resolve_and_record_inner`'s `Some(false)`
/// arm (store-only reconcile) could never lift. KIGI_SHARE_DIR-isolated (empty
/// store); KIGI_FOLDER_TRUST unset so the kill-switch is the only signal.
#[test]
#[serial_test::serial]
fn kill_switched_cold_cwd_stays_allowed_through_plugins_config_read() {
    use kigi_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("KIGI_SHARE_DIR", home.path());
    let _flag = EnvGuard::unset("KIGI_FOLDER_TRUST");
    let _sim = simulate_release_build();
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    let kigi = repo.path().join(".kigi");
    std::fs::create_dir_all(&kigi).unwrap();
    std::fs::write(kigi.join("config.toml"), "[plugins]\npaths = [\"./proj-plugin\"]\n")
        .unwrap();
    let cwd = repo.path();
    let remote = crate::util::config::RemoteSettings {
        folder_trust_enabled: Some(false),
        ..Default::default()
    };
    assert!(
        crate ::agent::folder_trust::resolve_and_record(cwd, Some(& remote), false),
        "kill-switch must resolve the cold key trusted"
    );
    let cfg = resolve_effective_plugins_config(cwd);
    assert!(
        cfg.paths.contains(& "./proj-plugin".to_string()),
        "kill-switched folder counts trusted, so the project path must merge"
    );
    assert!(
        crate ::agent::folder_trust::project_scope_allowed(cwd),
        "gate must still allow the kill-switched folder after the config read"
    );
}
