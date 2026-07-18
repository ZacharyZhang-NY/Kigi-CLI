use kigi_tools::computer::local::{LocalFs, LocalTerminalBackend};
use kigi_tools::computer::types::{AsyncFileSystem, TerminalBackend};
use kigi_tools::notification::ToolNotificationHandle;
use kigi_tools::registry::types::{SessionContext, ToolConfig, ToolServerConfig};
use serde_json::json;

#[tokio::test]
async fn web_search_errors_when_disabled() {
    let builder = crate::tools::bridge::ToolBridge::get_builder();
    let config = ToolServerConfig {
        tools: vec![ToolConfig {
            id: "Kigi:web_search".into(),
            params: None,
            name_override: None,
            params_name_overrides: None,
            description_override: None,
            behavior_version: None,
            kind: None,
        }],
        behavior_preset: None,
    };
    let fs: std::sync::Arc<dyn AsyncFileSystem> = std::sync::Arc::new(LocalFs);
    let terminal: std::sync::Arc<dyn TerminalBackend> =
        std::sync::Arc::new(LocalTerminalBackend::new());
    let ctx = SessionContext {
        backend: terminal,
        fs,
        cwd: std::env::temp_dir(),
        session_folder: std::env::temp_dir().join("kigi-web-search-disabled"),
        session_env: std::sync::Arc::new(std::collections::HashMap::new()),
        notification_handle: ToolNotificationHandle::noop(),
        owner_session_id: None,
        parent_scheduler_handle: None,
        skills: vec![],
        state_path: std::env::temp_dir().join("kigi-web-search-disabled/state.json"),
        memory_backend: None,
        web_search_config: kigi_tools::implementations::web_search::WebSearchConfig::Disabled,
        web_fetch_config: Default::default(),
        lsp: None,
        app_builder_deployer_config: Default::default(),
        api_key_provider: None,
        attribution_callback: None,
        system_reminder_tag: kigi_tools::reminders::DEFAULT_REMINDER_TAG,
    };
    let bridge = crate::tools::bridge::ToolBridge::finalize_builder(builder, config, ctx)
        .await
        .expect("finalize_builder should succeed");
    let result = bridge
        .call(
            "web_search",
            json!({
                "query": "test query"
            }),
            "web-search-disabled",
        )
        .await;
    assert!(result.is_err(), "web_search should fail when disabled");
}
