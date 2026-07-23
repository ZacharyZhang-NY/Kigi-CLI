//! Subagent configuration resolution crate.
//!
//! The pure-logic "resolution" phase of subagent spawning. Given a spawn
//! request and a resolution context (roles, personas, parent state), this
//! crate resolves:
//!
//! - Effective runtime config (model, persona, capability mode, isolation)
//!   via precedence: explicit override > role > persona > parent.
//! - Persona instruction loading (inline `instructions` + `instructions_file`).
//! - Role prompt file loading.
//! - Resume identity validation (type/persona match checks; model is soft-ignored).
//!
//! Nothing here may depend on session, coordinator, or transport types: local
//! hosts (e.g. `kigi-shell`) and any remote spawn path must both be able to
//! consume it.
//!
//! TODO: add a `resolve_subagent_spec()` composition entry point once shell
//! call sites move onto this crate. It needs `SubagentSpec` /
//! `ResolveSubagentRequest` / `ResolutionContext` boundary types, optional deps
//! for `AgentDefinition` lookup and worktree creation, the global > per-type >
//! role > parent model override chain, and capability mode filtering via
//! `SubagentCapabilityMode::filter_tool_config()`.

pub mod config;
pub mod context;
pub mod overrides;
pub mod resume;
pub mod types;

pub use config::{PersonaIOField, SubagentPersona, SubagentRole};
pub use overrides::resolve_effective_overrides;
pub use resume::{ResumeValidationError, validate_resume_identity};
pub use types::{ContextSource, EffectiveRuntimeConfig, ResolutionError, ResumeSourceData};
