//! Shared test utilities for Kigi crates.
//!
//! - **Hermetic git**: [`git::ensure_hermetic_git_on_path`] / [`require_git!`]
//! - **Repo helpers**: [`git::init_git_repo`], [`git::git_commit_all`]
//! - **Bazel runfiles**: [`crate_root!`]
//! - **Tracing capture**: [`tracing_capture::MessagePrefixCounter`]
//! - **Env knobs**: [`env::env_usize`]

pub mod env;
pub mod git;
pub mod image;
pub mod runfiles_util;
pub mod tracing_capture;
