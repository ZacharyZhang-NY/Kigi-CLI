//! `CodexListDirTool` — paginated, depth-limited, BFS directory listing.
//!
//! Ported from `codex-rs/core/src/tools/handlers/list_dir.rs`. Unlike the kigi
//! `ListDirTool`, it does not respect `.gitignore`, does not exclude hidden
//! files, and requires absolute paths.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crate::types::output::{ListDirContent, ListDirOutput};
use crate::types::requirements::Expr;
use crate::types::tool::{ToolKind, ToolNamespace};

const MAX_ENTRY_LENGTH: usize = 500;

const INDENTATION_SPACES: usize = 2;

const DESCRIPTION: &str =
    "Lists entries in a local directory with 1-indexed entry numbers and simple type labels.";

fn default_offset() -> usize {
    1
}
fn default_limit() -> usize {
    25
}
fn default_depth() -> usize {
    2
}

/// Input for the codex `list_dir` tool.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct CodexListDirInput {
    /// Absolute path to the directory to list.
    pub dir_path: String,

    /// The entry number to start listing from. Must be 1 or greater.
    #[serde(default = "default_offset")]
    pub offset: usize,

    /// The maximum number of entries to return.
    #[serde(default = "default_limit")]
    pub limit: usize,

    /// The maximum directory depth to traverse. Must be 1 or greater.
    #[serde(default = "default_depth")]
    pub depth: usize,
}

#[derive(Clone)]
struct DirEntry {
    /// Full relative path from the listing root; the sort key.
    name: String,
    /// Only the final path component.
    display_name: String,
    /// 0 for the root's direct children.
    depth: usize,
    kind: DirEntryKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DirEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

impl From<&std::fs::FileType> for DirEntryKind {
    fn from(ft: &std::fs::FileType) -> Self {
        // Symlink must be tested first: on Unix a symlink to a directory
        // answers true to both `is_symlink()` and `is_dir()`, and codex renders
        // such an entry with `@` rather than `/`.
        if ft.is_symlink() {
            DirEntryKind::Symlink
        } else if ft.is_dir() {
            DirEntryKind::Directory
        } else if ft.is_file() {
            DirEntryKind::File
        } else {
            DirEntryKind::Other
        }
    }
}

/// Codex-namespace list_dir tool — paginated, depth-limited directory listing.
#[derive(Debug, Default)]
pub struct CodexListDirTool;

async fn list_dir_slice(
    dir_path: &Path,
    offset: usize,
    limit: usize,
    depth: usize,
) -> Result<Vec<String>, String> {
    let mut entries = Vec::new();
    collect_entries(dir_path, Path::new(""), 0, depth, &mut entries)
        .await
        .map_err(|e| format!("Failed to read directory: {e}"))?;

    // Slash-normalized and case-sensitive, matching codex.
    entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));

    // An empty directory is a success case, not an error.
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    let total = entries.len();

    // offset is 1-indexed.
    let start_index = offset - 1;
    if start_index >= total {
        return Err("offset exceeds directory entry count".to_string());
    }

    let end_index = start_index.saturating_add(limit).min(total);
    let page = &entries[start_index..end_index];

    let mut lines: Vec<String> = page.iter().map(format_entry_line).collect();

    // Codex reports the page size actually returned, not the requested limit.
    if end_index < total {
        let capped_limit = end_index - start_index;
        lines.push(format!("More than {} entries found", capped_limit));
    }

    Ok(lines)
}

/// Collects entries breadth-first up to `max_depth` levels. Directories
/// beyond the depth limit are listed but not descended into.
///
/// The prefix carried through the queue is the raw joined path, never the
/// formatted name, so that `format_entry_name` truncation and separator
/// normalization cannot leak into deeper prefixes.
async fn collect_entries(
    dir_path: &Path,
    relative_prefix: &Path,
    current_depth: usize,
    max_depth: usize,
    entries: &mut Vec<DirEntry>,
) -> Result<(), std::io::Error> {
    // (absolute path, raw relative prefix, depth)
    let mut queue: VecDeque<(PathBuf, PathBuf, usize)> = VecDeque::new();
    queue.push_back((
        dir_path.to_path_buf(),
        relative_prefix.to_path_buf(),
        current_depth,
    ));

    while let Some((abs_path, rel_prefix, depth)) = queue.pop_front() {
        let mut read_dir = tokio::fs::read_dir(&abs_path).await?;

        // (entry, absolute path, raw relative path for the next level)
        let mut children: Vec<(DirEntry, PathBuf, PathBuf)> = Vec::new();
        while let Some(entry) = read_dir.next_entry().await? {
            let file_type = entry.file_type().await?;
            let kind = DirEntryKind::from(&file_type);

            let raw_name = entry.file_name();
            let display_name = format_entry_component(&raw_name);

            let entry_relative_path = rel_prefix.join(&raw_name);
            // Sorting is done on the formatted name, not the raw path.
            let name = format_entry_name(&entry_relative_path.to_string_lossy());

            children.push((
                DirEntry {
                    name,
                    display_name,
                    depth,
                    kind,
                },
                entry.path(),
                entry_relative_path,
            ));
        }

        // Sort children so the traversal order is deterministic.
        children.sort_unstable_by(|a, b| a.0.name.cmp(&b.0.name));

        for (dir_entry, child_abs_path, child_raw_rel) in children {
            let is_dir = dir_entry.kind == DirEntryKind::Directory;
            entries.push(dir_entry);

            if is_dir && depth + 1 < max_depth {
                queue.push_back((child_abs_path, child_raw_rel, depth + 1));
            }
        }
    }

    Ok(())
}

fn format_entry_line(entry: &DirEntry) -> String {
    let indent = " ".repeat(entry.depth * INDENTATION_SPACES);
    let suffix = match entry.kind {
        DirEntryKind::Directory => "/",
        DirEntryKind::Symlink => "@",
        DirEntryKind::Other => "?",
        DirEntryKind::File => "",
    };
    format!("{}{}{}", indent, entry.display_name, suffix)
}

/// Normalizes backslashes to forward slashes, then truncates.
fn format_entry_name(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    take_at_char_boundary(&normalized, MAX_ENTRY_LENGTH).to_string()
}

fn format_entry_component(name: &std::ffi::OsStr) -> String {
    let s = name.to_string_lossy();
    take_at_char_boundary(&s, MAX_ENTRY_LENGTH).to_string()
}

/// Yields at most `max_bytes` bytes, cut back to a char boundary.
fn take_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    let end = crate::util::floor_char_boundary(s, max_bytes);
    &s[..end]
}

impl crate::types::tool_metadata::ToolMetadata for CodexListDirTool {
    fn kind(&self) -> ToolKind {
        ToolKind::ListDir
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Codex
    }

    fn description_template(&self) -> &str {
        DESCRIPTION
    }

    fn requires_expr(&self) -> Expr<crate::types::requirements::ToolRequirement> {
        Expr::True
    }
}

impl kigi_tool_runtime::Tool for CodexListDirTool {
    type Args = CodexListDirInput;
    type Output = ListDirOutput;

    fn id(&self) -> kigi_tool_protocol::ToolId {
        kigi_tool_protocol::ToolId::new("list_dir").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::kigi_tool_runtime::ListToolsContext,
    ) -> kigi_tool_types::ToolDescription {
        kigi_tool_types::ToolDescription::new(
            "list_dir",
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> kigi_tool_protocol::ToolCapabilities {
        kigi_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(kigi_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(
        name = "tool.codex_list_dir",
        skip_all,
        fields(path = %input.dir_path)
    )]
    async fn run(
        &self,
        _ctx: kigi_tool_runtime::ToolCallContext,
        input: CodexListDirInput,
    ) -> Result<ListDirOutput, kigi_tool_runtime::ToolError> {
        let CodexListDirInput {
            dir_path,
            offset,
            limit,
            depth,
        } = input;

        if offset == 0 {
            return Ok(ListDirOutput::Error(
                "offset must be a 1-indexed entry number".to_string(),
            ));
        }
        if limit == 0 {
            return Ok(ListDirOutput::Error(
                "limit must be greater than zero".to_string(),
            ));
        }
        if depth == 0 {
            return Ok(ListDirOutput::Error(
                "depth must be greater than zero".to_string(),
            ));
        }
        let path = PathBuf::from(&dir_path);
        if !path.is_absolute() {
            return Ok(ListDirOutput::Error(
                "dir_path must be an absolute path".to_string(),
            ));
        }

        let entries = match list_dir_slice(&path, offset, limit, depth).await {
            Ok(entries) => entries,
            Err(msg) => return Ok(ListDirOutput::Error(msg)),
        };

        let mut output = Vec::with_capacity(entries.len() + 1);
        output.push(format!("Absolute path: {}", path.display()));
        output.extend(entries);
        let content = output.join("\n");

        Ok(ListDirOutput::Content(ListDirContent {
            content,
            absolute_root_path: path,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn lists_directory_entries() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("file.txt"), "content").unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("file.txt"), tmp.path().join("link")).unwrap();

        let result = list_dir_slice(tmp.path(), 1, 25, 2).await.unwrap();

        let joined = result.join("\n");
        assert!(joined.contains("file.txt"), "missing file.txt in: {joined}");
        assert!(joined.contains("subdir/"), "missing subdir/ in: {joined}");
        #[cfg(unix)]
        assert!(joined.contains("link@"), "missing link@ in: {joined}");
    }

    #[tokio::test]
    async fn errors_when_offset_exceeds_entries() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();

        let err = list_dir_slice(tmp.path(), 100, 25, 2).await.unwrap_err();
        assert_eq!(err, "offset exceeds directory entry count");
    }

    #[tokio::test]
    async fn respects_depth_parameter() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("a");
        let subsub = sub.join("b");
        std::fs::create_dir_all(&subsub).unwrap();
        std::fs::write(subsub.join("deep.txt"), "").unwrap();

        let depth1 = list_dir_slice(tmp.path(), 1, 100, 1).await.unwrap();
        let joined1 = depth1.join("\n");
        assert!(joined1.contains("a/"), "should see dir a/");
        assert!(!joined1.contains("b/"), "should NOT see b/ at depth 1");
        assert!(
            !joined1.contains("deep.txt"),
            "should NOT see deep.txt at depth 1"
        );

        let depth2 = list_dir_slice(tmp.path(), 1, 100, 2).await.unwrap();
        let joined2 = depth2.join("\n");
        assert!(joined2.contains("a/"), "should see dir a/");
        assert!(joined2.contains("b/"), "should see b/ at depth 2");
        assert!(
            !joined2.contains("deep.txt"),
            "should NOT see deep.txt at depth 2"
        );

        let depth3 = list_dir_slice(tmp.path(), 1, 100, 3).await.unwrap();
        let joined3 = depth3.join("\n");
        assert!(joined3.contains("a/"), "should see dir a/");
        assert!(joined3.contains("b/"), "should see b/ at depth 3");
        assert!(
            joined3.contains("deep.txt"),
            "should see deep.txt at depth 3"
        );
    }

    #[tokio::test]
    async fn paginates_in_sorted_order() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("c.txt"), "").unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "").unwrap();

        let page1 = list_dir_slice(tmp.path(), 1, 2, 1).await.unwrap();
        assert!(page1[0].contains("a.txt"), "first entry should be a.txt");
        assert!(page1[1].contains("b.txt"), "second entry should be b.txt");
        assert!(
            page1.last().unwrap().contains("More than 2 entries found"),
            "should have overflow message"
        );

        let page2 = list_dir_slice(tmp.path(), 3, 2, 1).await.unwrap();
        assert_eq!(page2.len(), 1, "second page should have 1 entry");
        assert!(page2[0].contains("c.txt"), "should be c.txt");
    }

    #[tokio::test]
    async fn handles_large_limit_without_overflow() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("only.txt"), "").unwrap();

        let result = list_dir_slice(tmp.path(), 1, usize::MAX, 1).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("only.txt"));
    }

    #[tokio::test]
    async fn indicates_truncated_results() {
        let tmp = TempDir::new().unwrap();
        for i in 0..40 {
            std::fs::write(tmp.path().join(format!("file_{:03}.txt", i)), "").unwrap();
        }

        let result = list_dir_slice(tmp.path(), 1, 25, 1).await.unwrap();
        assert!(
            result
                .last()
                .unwrap()
                .contains("More than 25 entries found"),
            "should indicate truncation"
        );
    }

    #[tokio::test]
    async fn truncation_respects_sorted_order() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("z.txt"), "").unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::write(tmp.path().join("m.txt"), "").unwrap();

        let result = list_dir_slice(tmp.path(), 2, 1, 1).await.unwrap();
        assert!(result[0].contains("m.txt"), "offset=2 should land on m.txt");
    }

    #[tokio::test]
    async fn tool_lists_directory() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), "world").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let tool = CodexListDirTool;
        let ctx = kigi_tool_runtime::ToolCallContext::default();

        let input = CodexListDirInput {
            dir_path: tmp.path().to_string_lossy().to_string(),
            offset: 1,
            limit: 25,
            depth: 2,
        };

        let result = kigi_tool_runtime::Tool::run(&tool, ctx, input)
            .await
            .unwrap();
        match result {
            ListDirOutput::Content(content) => {
                assert!(
                    content.content.contains("Absolute path:"),
                    "should have header"
                );
                assert!(content.content.contains("hello.txt"), "should list file");
                assert!(content.content.contains("sub/"), "should list dir");
                assert_eq!(content.absolute_root_path, tmp.path());
            }
            other => panic!("Expected Content, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_returns_error_for_invalid_offset() {
        let tmp = TempDir::new().unwrap();
        let tool = CodexListDirTool;
        let ctx = kigi_tool_runtime::ToolCallContext::default();

        let input = CodexListDirInput {
            dir_path: tmp.path().to_string_lossy().to_string(),
            offset: 0,
            limit: 25,
            depth: 2,
        };

        let result = kigi_tool_runtime::Tool::run(&tool, ctx, input)
            .await
            .unwrap();
        match result {
            ListDirOutput::Error(msg) => {
                assert_eq!(msg, "offset must be a 1-indexed entry number");
            }
            other => panic!("Expected Error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_returns_error_for_nonexistent_dir() {
        let tmp = TempDir::new().unwrap();
        let tool = CodexListDirTool;
        let ctx = kigi_tool_runtime::ToolCallContext::default();

        let input = CodexListDirInput {
            dir_path: tmp.path().join("nonexistent").to_string_lossy().to_string(),
            offset: 1,
            limit: 25,
            depth: 2,
        };

        let result = kigi_tool_runtime::Tool::run(&tool, ctx, input)
            .await
            .unwrap();
        match result {
            ListDirOutput::Error(msg) => {
                assert!(
                    msg.contains("Failed to read directory"),
                    "Expected filesystem error, got: {msg}"
                );
            }
            other => panic!("Expected Error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_returns_error_for_relative_path() {
        let tool = CodexListDirTool;
        let ctx = kigi_tool_runtime::ToolCallContext::default();

        let input = CodexListDirInput {
            dir_path: "relative/path".to_string(),
            offset: 1,
            limit: 25,
            depth: 2,
        };

        let result = kigi_tool_runtime::Tool::run(&tool, ctx, input)
            .await
            .unwrap();
        match result {
            ListDirOutput::Error(msg) => {
                assert_eq!(msg, "dir_path must be an absolute path");
            }
            other => panic!("Expected Error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_returns_error_for_zero_limit() {
        let tmp = TempDir::new().unwrap();
        let tool = CodexListDirTool;
        let ctx = kigi_tool_runtime::ToolCallContext::default();

        let input = CodexListDirInput {
            dir_path: tmp.path().to_string_lossy().to_string(),
            offset: 1,
            limit: 0,
            depth: 2,
        };

        let result = kigi_tool_runtime::Tool::run(&tool, ctx, input)
            .await
            .unwrap();
        match result {
            ListDirOutput::Error(msg) => {
                assert_eq!(msg, "limit must be greater than zero");
            }
            other => panic!("Expected Error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_returns_error_for_zero_depth() {
        let tmp = TempDir::new().unwrap();
        let tool = CodexListDirTool;
        let ctx = kigi_tool_runtime::ToolCallContext::default();

        let input = CodexListDirInput {
            dir_path: tmp.path().to_string_lossy().to_string(),
            offset: 1,
            limit: 25,
            depth: 0,
        };

        let result = kigi_tool_runtime::Tool::run(&tool, ctx, input)
            .await
            .unwrap();
        match result {
            ListDirOutput::Error(msg) => {
                assert_eq!(msg, "depth must be greater than zero");
            }
            other => panic!("Expected Error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_directory_returns_success() {
        let tmp = TempDir::new().unwrap();

        let result = list_dir_slice(tmp.path(), 1, 25, 2).await.unwrap();
        assert!(result.is_empty(), "empty dir should return empty vec");

        let tool = CodexListDirTool;
        let ctx = kigi_tool_runtime::ToolCallContext::default();
        let input = CodexListDirInput {
            dir_path: tmp.path().to_string_lossy().to_string(),
            offset: 1,
            limit: 25,
            depth: 2,
        };

        let output = kigi_tool_runtime::Tool::run(&tool, ctx, input)
            .await
            .unwrap();
        match output {
            ListDirOutput::Content(content) => {
                assert!(
                    content.content.contains("Absolute path:"),
                    "should have header even for empty dir"
                );
            }
            other => panic!("Empty directory should not error, got: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_to_directory_classified_as_symlink() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("real_dir")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real_dir"), tmp.path().join("link_to_dir"))
            .unwrap();

        let result = list_dir_slice(tmp.path(), 1, 25, 1).await.unwrap();
        let joined = result.join("\n");

        assert!(
            joined.contains("link_to_dir@"),
            "symlink-to-dir should render with @ suffix, got: {joined}"
        );
        assert!(
            joined.contains("real_dir/"),
            "real dir should render with / suffix, got: {joined}"
        );
    }

    #[tokio::test]
    async fn capped_limit_in_overflow_message() {
        let tmp = TempDir::new().unwrap();
        for i in 0..30 {
            std::fs::write(tmp.path().join(format!("f_{:03}.txt", i)), "").unwrap();
        }

        let result = list_dir_slice(tmp.path(), 1, 10, 1).await.unwrap();
        let last = result.last().unwrap();
        assert!(
            last.contains("More than 10 entries found"),
            "overflow message should use capped limit, got: {last}"
        );
    }
}
