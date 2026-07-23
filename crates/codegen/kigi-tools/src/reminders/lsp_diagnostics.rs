//! Cross-cutting reminder: notifies LSP of file changes and drains diagnostics.

use std::sync::Arc;

use crate::implementations::lsp::LspBackend;
use crate::types::output::{SearchReplaceOutput, ToolOutput};
use crate::types::resources::SharedResources;
use crate::types::tool::Reminder;

pub struct LspDiagnosticsReminder;

#[async_trait::async_trait]
impl Reminder for LspDiagnosticsReminder {
    async fn collect_reminders(
        &self,
        resources: SharedResources,
        tool_output: &ToolOutput,
    ) -> Vec<String> {
        let lsp = {
            let res = resources.lock().await;
            match res.get::<Arc<dyn LspBackend>>() {
                Some(h) => h.clone(),
                None => return vec![],
            }
        };

        lsp.ensure_started_background();

        // Notify LSP of the edit so diagnostics refresh; when the adapter is not
        // yet ready the notification is buffered rather than dropped.
        if let ToolOutput::SearchReplace(SearchReplaceOutput::EditsApplied(edits)) = tool_output
            && let Ok(content) = std::fs::read_to_string(&edits.absolute_path)
        {
            lsp.notify_file_changed(&edits.absolute_path, &content)
                .await;
        }

        // Drain diagnostics queued by this or earlier edits.
        if let Some(summary) = lsp
            .drain_diagnostics(std::time::Duration::from_millis(500))
            .await
        {
            return vec![summary.text];
        }

        vec![]
    }
}
