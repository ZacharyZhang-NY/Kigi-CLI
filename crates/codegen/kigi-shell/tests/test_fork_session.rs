//! Integration tests for the fork session flow.

use agent_client_protocol as acp;
use kigi_shell::sampling::ConversationItem;
use kigi_shell::session::info::Info;
use kigi_shell::session::storage::{JsonlStorageAdapter, StorageAdapter};
use tempfile::TempDir;

async fn create_test_session(storage: &JsonlStorageAdapter, session_id: &str, cwd: &str) -> Info {
    let info = Info {
        id: acp::SessionId::new(session_id),
        cwd: cwd.to_string(),
    };

    let model_id = acp::ModelId::new("kigi-code-fast-1");
    storage.init_session(&info, model_id).await.unwrap();

    let msg = ConversationItem::user("Hello world");
    storage.append_chat_message(&info, &msg).await.unwrap();

    let notification = acp::SessionNotification::new(
        acp::SessionId::new(session_id),
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new("Test response".to_string()),
        ))),
    );
    storage
        .append_update(
            &info,
            &kigi_shell::session::storage::SessionUpdate::Acp(Box::new(notification)),
        )
        .await
        .unwrap();

    info
}

#[tokio::test]
async fn test_fork_session_creates_new_session_with_parent_tracking() {
    let temp_dir = TempDir::new().unwrap();
    let storage = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = create_test_session(&storage, "source-session-123", "/source/path").await;

    let target_info = Info {
        id: acp::SessionId::new("fork-session-456"),
        cwd: "/new/path".to_string(),
    };

    let options = kigi_shell::session::storage::CopySessionOptions {
        parent_session_id: Some("source-session-123".to_string()),
        new_model_id: Some("kigi-3".to_string()),
        target_prompt_index: None,
        ..Default::default()
    };

    let result = storage
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();

    assert_eq!(result.chat_messages_copied, 1);
    assert_eq!(result.updates_copied, 1);

    let loaded = storage.load_session(&target_info).await.unwrap();

    assert_eq!(loaded.summary.info.id.to_string(), "fork-session-456");
    assert_eq!(loaded.summary.info.cwd, "/new/path");
    assert_eq!(loaded.summary.current_model_id, acp::ModelId::new("kigi-3"));
    assert_eq!(
        loaded.summary.parent_session_id,
        Some("source-session-123".to_string())
    );
    assert!(loaded.summary.forked_at.is_some());

    assert_eq!(loaded.chat_history.len(), 1);

    assert_eq!(loaded.updates.len(), 1);
    match &loaded.updates[0] {
        kigi_shell::session::storage::SessionUpdate::Acp(notification) => {
            assert_eq!(notification.session_id.to_string(), "fork-session-456");
        }
        _ => panic!("Expected ACP update"),
    }
}

#[tokio::test]
async fn test_fork_preserves_session_title() {
    let temp_dir = TempDir::new().unwrap();
    let storage = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = create_test_session(&storage, "titled-session", "/source").await;

    storage
        .update_session_title(&source_info, "My Important Session".to_string())
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("fork-titled"),
        cwd: "/new".to_string(),
    };

    let options = kigi_shell::session::storage::CopySessionOptions {
        parent_session_id: Some("titled-session".to_string()),
        new_model_id: None,
        target_prompt_index: None,
        ..Default::default()
    };

    storage
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();

    // display_title() surfaces generated_title, the LLM-set title field.
    let loaded = storage.load_session(&target_info).await.unwrap();
    assert_eq!(loaded.summary.display_title(), "My Important Session");
}
