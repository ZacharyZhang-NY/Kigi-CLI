//! Endpoint defaults live in the [`kigi_env`] leaf crate so sibling crates can
//! share them without depending on this one; only the shared test helper is
//! re-exported here.
pub use kigi_env::EnvVarGuard;
