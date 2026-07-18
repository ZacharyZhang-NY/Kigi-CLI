//! MCP descriptor mirror.
//!
//! Some templates read MCP metadata from an on-disk descriptor tree. Keep that
//! tree current as servers connect by (re)writing descriptors for connected
//! servers on every MCP tool-set change, not just at the first turn.
//!
//! Local MCP writes are upsert-only — folders for servers removed mid-session are
//! not pruned (cleaned on the next session's first-turn build); pruning against
//! an async-changing client set risks deleting a just-connected server's folder.
//!
//! Owning the descriptor I/O here keeps `acp_session.rs` thin.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::session::mcp_servers::{McpClient, sanitize_descriptor_segment};

/// Per-server descriptor folder: `<mcps_root>/<sanitized server name>`. Uses the
/// sanitizer shared with `kigi-mcp` so the advertised folder matches disk.
pub(crate) fn server_descriptor_dir(mcps_root: &Path, server_name: &str) -> PathBuf {
    mcps_root.join(sanitize_descriptor_segment(server_name))
}

/// Upsert the on-disk tool descriptors for the given connected clients.
///
/// Safe to run concurrently (the first-turn build and the background handshake
/// task can both call it): `materialize_descriptors` writes each file
/// atomically, so overlapping writers converge without a lock. Errors are
/// logged, not propagated.
pub(crate) async fn materialize_descriptors_for_clients(
    mcps_root: &Path,
    clients: Vec<(String, Arc<McpClient>)>,
) {
    for (name, client) in clients {
        let server_dir = server_descriptor_dir(mcps_root, &name);
        if let Err(e) = client.materialize_descriptors(&server_dir).await {
            tracing::warn!(
                server = %name,
                path = %server_dir.display(),
                error = %e,
                "failed to materialize MCP descriptors",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_unsafe_chars_and_never_empty() {
        assert_eq!(sanitize_descriptor_segment("a/b:c d"), "a_b_c_d");
        assert_eq!(sanitize_descriptor_segment(""), "_");
        assert_eq!(sanitize_descriptor_segment("keep-1.2_x"), "keep-1.2_x");
    }

    #[test]
    fn server_dir_is_joined_under_root() {
        let root = Path::new("/home/u/.kigi/projects/enc/mcps");
        assert_eq!(server_descriptor_dir(root, "vercel"), root.join("vercel"));
    }
}
