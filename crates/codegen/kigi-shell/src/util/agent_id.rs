//! Stable per-install agent identifier.
//!
//! Stamped on requests (`x-grok-agent-id` / `x_grok_agent_id`) so the backend
//! can bucket by install. Cached in `$KIGI_SHARE_DIR/agent_id` so every process on
//! this install (and restarts) agree; the in-memory `OnceLock` makes repeat
//! calls free.

use std::sync::OnceLock;

/// Cached agent ID — stored in memory after first load.
static AGENT_ID: OnceLock<String> = OnceLock::new();
/// Cached agent instance ID — per-process lifetime.
static AGENT_INSTANCE_ID: OnceLock<String> = OnceLock::new();

/// Returns the per-install agent ID, backed by a file cache under the grok
/// home so it is stable across process restarts.
pub fn agent_id() -> String {
    AGENT_ID.get_or_init(load_or_compute_agent_id).clone()
}

/// Returns a per-process agent instance ID: stable within one process,
/// new on process restart.
pub fn agent_instance_id() -> String {
    AGENT_INSTANCE_ID
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .clone()
}

fn load_or_compute_agent_id() -> String {
    let cache_path = crate::util::kigi_home::kigi_home().join("agent_id");

    // Try to read from the cache file first (fast path).
    if let Ok(cached) = std::fs::read_to_string(&cache_path) {
        let cached = cached.trim();
        if !cached.is_empty() {
            return cached.to_string();
        }
    }

    let id = uuid::Uuid::new_v4().to_string();

    // Save to the cache file (best effort, ignore errors).
    let _ = std::fs::write(&cache_path, &id);

    id
}
