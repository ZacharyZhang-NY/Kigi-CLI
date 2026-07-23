//! System prompts for built-in subagent profiles.
//!
//! Tool names inside these prompts are never hardcoded: they are
//! `${{ tools.by_kind.* }}` template variables that MiniJinja resolves to the
//! session's actual tool names during `ToolBridge::render_prompt()`, so they
//! follow name overrides and alternate namespaces. A kind that is absent from
//! the renderer context resolves to an empty string, which is why prompts guard
//! whole sections with `${%- if tools.by_kind.X %}`.

pub use kigi_tool_types::{EXPLORE_PROMPT, GENERAL_PURPOSE_PROMPT, PLAN_PROMPT};
