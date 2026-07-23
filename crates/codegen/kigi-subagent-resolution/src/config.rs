//! Subagent role and persona configuration types.
//!
//! These are the canonical definitions; the shell re-exports them via
//! `kigi_shell::config`. Filesystem discovery and config layering
//! (CLI > env > TOML > remote) stay in `kigi-shell` on `SubagentsConfig` —
//! this crate only ever sees already-resolved maps.

use kigi_tools::implementations::skills::discovery::extract_first_paragraph;
use std::path::PathBuf;

use serde::Deserialize;

/// A declarative subagent role definition from config.
///
/// Roles are named presets callers reference via the `subagent_type` field in
/// the task tool. Every default here can be overridden per-spawn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SubagentRole {
    pub description: String,
    /// One of: "read-only", "read-write", "execute", "all".
    #[serde(default)]
    pub default_capability_mode: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// One of: "low", "medium", "high".
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Loaded at spawn time and prepended to the child's prompt as a
    /// `<role-instructions>` block.
    #[serde(default)]
    pub prompt_file: Option<String>,
    /// One of: "none", "worktree".
    #[serde(default)]
    pub default_isolation: Option<String>,
    /// Base directory for resolving a relative `prompt_file`: the parent dir
    /// of the source `.toml`, filled in during discovery.
    #[serde(skip)]
    pub source_dir: Option<PathBuf>,
}

/// A named persona/SOUL definition controlling tone, style, and behavior.
///
/// Personas are referenced by name via the `persona` field in the task tool.
/// Their instructions are prepended to the child's prompt as a `<persona>`
/// XML block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SubagentPersona {
    pub instructions: Option<String>,
    /// When absent, summaries fall back to the first paragraph of
    /// `instructions`.
    pub description: Option<String>,
    /// Loaded at spawn time and merged with `instructions`, which is prepended
    /// before the file content when both are set.
    pub instructions_file: Option<String>,
    #[serde(default)]
    pub inputs: Vec<PersonaIOField>,
    #[serde(default)]
    pub outputs: Vec<PersonaIOField>,
    /// One of: "none", "worktree".
    #[serde(default)]
    pub default_isolation: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// One of: "low", "medium", "high".
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Base directory for resolving relative file references: the parent dir
    /// of the source `.toml`, filled in during discovery. When `None`,
    /// relative paths resolve against the workspace cwd.
    #[serde(skip)]
    pub source_dir: Option<PathBuf>,
    /// Absolute path of the source file, filled in during discovery. `None`
    /// for personas declared inline in config.
    #[serde(skip)]
    pub source_path: Option<String>,
}

/// A declared input or output for a persona.
///
/// Lets the parent agent discover what a persona needs and what it produces
/// without hardcoded knowledge of the persona's protocol.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersonaIOField {
    pub name: String,
    /// Kind of artifact: "file", "text", etc.
    #[serde(default = "PersonaIOField::default_io_type")]
    pub io_type: String,
    #[serde(default)]
    pub required: bool,
    pub description: String,
}

impl PersonaIOField {
    fn default_io_type() -> String {
        "file".to_string()
    }
}

impl SubagentPersona {
    /// Renders this persona's IO contract as Markdown for the task tool
    /// description.
    pub fn render_io_summary(&self, name: &str) -> String {
        let fallback;
        let desc = if let Some(d) = self.description.as_deref().filter(|s| !s.trim().is_empty()) {
            d
        } else {
            fallback = self
                .instructions
                .as_deref()
                .and_then(extract_first_paragraph);
            fallback.as_deref().unwrap_or("Custom persona")
        };
        let scope = match self.source_path.as_deref() {
            Some(path) if path.contains("/bundled/") => "[bundled]",
            Some(_) => "[user]",
            None => "[local]",
        };
        let mut lines = vec![format!("- **{name}** {scope}: {desc}")];
        if let Some(ref path) = self.source_path {
            lines.push(format!("  Path: {path}"));
        }
        if !self.inputs.is_empty() {
            lines.push("    Expects in prompt:".to_string());
            for io in &self.inputs {
                let req = if io.required { "REQUIRED" } else { "optional" };
                lines.push(format!(
                    "      - `{}` ({}, {}): {}",
                    io.name, io.io_type, req, io.description
                ));
            }
        }
        if !self.outputs.is_empty() {
            lines.push("    Produces:".to_string());
            for io in &self.outputs {
                let req = if io.required { "REQUIRED" } else { "optional" };
                lines.push(format!(
                    "      - `{}` ({}, {}): {}",
                    io.name, io.io_type, req, io.description
                ));
            }
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_role_deserialize_defaults() {
        let role: SubagentRole = toml::from_str("").unwrap();
        assert_eq!(role.description, "");
        assert!(role.default_capability_mode.is_none());
        assert!(role.model.is_none());
        assert!(role.prompt_file.is_none());
    }

    #[test]
    fn subagent_role_deserialize_full() {
        let toml_str = r#"
description = "Research agent"
default_capability_mode = "read-only"
model = "kigi-3"
reasoning_effort = "high"
prompt_file = ".kigi/prompts/researcher.md"
default_isolation = "worktree"
"#;
        let role: SubagentRole = toml::from_str(toml_str).unwrap();
        assert_eq!(role.description, "Research agent");
        assert_eq!(role.default_capability_mode.as_deref(), Some("read-only"));
        assert_eq!(role.model.as_deref(), Some("kigi-3"));
        assert_eq!(role.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            role.prompt_file.as_deref(),
            Some(".kigi/prompts/researcher.md")
        );
        assert_eq!(role.default_isolation.as_deref(), Some("worktree"));
    }

    #[test]
    fn subagent_persona_deserialize_defaults() {
        let persona: SubagentPersona = toml::from_str("").unwrap();
        assert!(persona.instructions.is_none());
        assert!(persona.description.is_none());
        assert!(persona.instructions_file.is_none());
        assert!(persona.inputs.is_empty());
        assert!(persona.outputs.is_empty());
    }

    #[test]
    fn subagent_persona_deserialize_full() {
        let toml_str = r#"
instructions = "You are a concise writer."
description = "A concise writing persona."
instructions_file = ".kigi/personas/concise.md"
model = "kigi-3-fast"
reasoning_effort = "low"
default_isolation = "none"

[[inputs]]
name = "review_file"
io_type = "file"
required = true
description = "Path to the review notes file"

[[outputs]]
name = "summary_file"
io_type = "file"
required = false
description = "Path to write the summary"
"#;
        let persona: SubagentPersona = toml::from_str(toml_str).unwrap();
        assert_eq!(
            persona.instructions.as_deref(),
            Some("You are a concise writer.")
        );
        assert_eq!(
            persona.instructions_file.as_deref(),
            Some(".kigi/personas/concise.md")
        );
        assert_eq!(persona.model.as_deref(), Some("kigi-3-fast"));
        assert_eq!(persona.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(persona.inputs.len(), 1);
        assert_eq!(persona.inputs[0].name, "review_file");
        assert!(persona.inputs[0].required);
        assert_eq!(persona.outputs.len(), 1);
        assert_eq!(persona.outputs[0].name, "summary_file");
        assert!(!persona.outputs[0].required);
        assert_eq!(
            persona.description.as_deref(),
            Some("A concise writing persona.")
        );
    }

    #[test]
    fn persona_io_field_default_io_type_is_file() {
        let json = r#"{"name": "test", "description": "a test field"}"#;
        let field: PersonaIOField = serde_json::from_str(json).unwrap();
        assert_eq!(field.io_type, "file");
        assert!(!field.required);
    }

    #[test]
    fn render_io_summary_uses_explicit_description() {
        let persona = SubagentPersona {
            description: Some("A focused code reviewer.".to_owned()),
            instructions: Some("Ignore this line.\nAnd this one.".to_owned()),
            ..Default::default()
        };
        let summary = persona.render_io_summary("reviewer");
        assert!(summary.contains("A focused code reviewer."));
        assert!(!summary.contains("Ignore this line"));
    }

    #[test]
    fn render_io_summary_extracts_first_paragraph_from_instructions() {
        let persona = SubagentPersona {
            instructions: Some(
                "You are a meticulous code reviewer. Review code and produce structured review\n\
                 notes in a Markdown file at the path given in the prompt.\n\n\
                 Process:\n1. Read the code."
                    .to_owned(),
            ),
            ..Default::default()
        };
        let summary = persona.render_io_summary("reviewer");
        assert!(
            summary.contains("You are a meticulous code reviewer. Review code and produce structured review notes in a Markdown file at the path given in the prompt."),
            "should join multi-line first paragraph: {summary}"
        );
        assert!(!summary.contains("Process"));
    }

    #[test]
    fn render_io_summary_falls_back_to_custom_persona() {
        let persona = SubagentPersona::default();
        let summary = persona.render_io_summary("empty");
        assert!(summary.contains("Custom persona"));
    }

    #[test]
    fn render_io_summary_extracts_lead_paragraph_before_list() {
        let persona = SubagentPersona {
            instructions: Some(
                "You are a thorough researcher. When exploring a question:\n\
                 - Exhaust all reasonable search avenues before concluding\n\
                 - Always cite specific file paths"
                    .to_owned(),
            ),
            ..Default::default()
        };
        let summary = persona.render_io_summary("researcher");
        assert!(summary.contains("You are a thorough researcher. When exploring a question:"));
        assert!(!summary.contains("Always cite specific file paths"));
    }

    #[test]
    fn render_io_summary_headings_only_instructions_falls_back() {
        let persona = SubagentPersona {
            instructions: Some("# Heading\n## Sub".to_owned()),
            ..Default::default()
        };
        let summary = persona.render_io_summary("test");
        assert!(summary.contains("Custom persona"));
    }

    #[test]
    fn render_io_summary_empty_description_falls_through_to_instructions() {
        let persona = SubagentPersona {
            description: Some("".to_owned()),
            instructions: Some("Actual description here.".to_owned()),
            ..Default::default()
        };
        let summary = persona.render_io_summary("test");
        assert!(summary.contains("Actual description here."));
    }

    #[test]
    fn render_io_summary_whitespace_description_falls_through_to_instructions() {
        let persona = SubagentPersona {
            description: Some("   ".to_owned()),
            instructions: Some("Real content.".to_owned()),
            ..Default::default()
        };
        let summary = persona.render_io_summary("test");
        assert!(summary.contains("Real content."));
    }
}
