//! Environment helpers for the shell crate family.
//!
//! Kigi has exactly one environment; endpoint defaults live in the
//! [`kigi_env`] leaf crate so sibling crates can share them without
//! depending on this crate. This module re-exports the shared test
//! helper.
pub use kigi_env::EnvVarGuard;
