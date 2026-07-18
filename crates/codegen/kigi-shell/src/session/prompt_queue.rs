//! Server-authoritative prompt queue wire types.
//!
//! Canonical definitions live in `kigi_prompt_queue`; this re-export keeps every existing
//! `crate::session::prompt_queue::*` and cross-crate `kigi_shell::session::prompt_queue::*`
//! path resolving without edits.

pub use kigi_prompt_queue::{QueueChanged, QueueEntryMeta, QueueEntryWire};

// Outbound method for broadcast_queue_changed. An ACP routing concern, not a queue concern.
pub const QUEUE_CHANGED_METHOD: &str = "kigi/queue/changed";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_changed_serializes_camel_case_with_session_id() {
        let payload = QueueChanged {
            session_id: "sess-1".to_string(),
            entries: vec![QueueEntryWire {
                id: "p1".to_string(),
                version: 0,
                owner: Some("kigi-tui".to_string()),
                last_editor: None,
                kind: "prompt".to_string(),
                text: "hello".to_string(),
                position: 0,
            }],
            running_prompt_id: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["sessionId"], "sess-1");
        assert_eq!(json["entries"][0]["id"], "p1");
        assert_eq!(json["entries"][0]["position"], 0);
        assert!(json["entries"][0].get("lastEditor").is_none());
        assert!(json.get("runningPromptId").is_none());
        let round: QueueChanged = serde_json::from_value(json).unwrap();
        assert_eq!(round, payload);
    }

    #[test]
    fn queue_changed_round_trips_running_prompt_id() {
        let payload = QueueChanged {
            session_id: "sess-1".to_string(),
            entries: Vec::new(),
            running_prompt_id: Some("prompt-running".to_string()),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["runningPromptId"], "prompt-running");
        let round: QueueChanged = serde_json::from_value(json).unwrap();
        assert_eq!(round, payload);
    }

    #[test]
    fn queue_entry_wire_round_trips_last_editor() {
        let entry = QueueEntryWire {
            id: "p1".to_string(),
            version: 3,
            owner: Some("kigi-tui".to_string()),
            last_editor: Some("kigi-vscode".to_string()),
            kind: "prompt".to_string(),
            text: "hello".to_string(),
            position: 0,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["lastEditor"], "kigi-vscode");
        let round: QueueEntryWire = serde_json::from_value(json).unwrap();
        assert_eq!(round, entry);
    }
}
