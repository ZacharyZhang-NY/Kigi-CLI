//! Version-specific behavior modules for `list_dir`. Current behavior (BFS
//! budget rendering, structured error variants) lives in `list_dir/mod.rs`;
//! `legacy_0_4_10` holds depth-threshold rendering and generic error messages.

pub(crate) mod legacy_0_4_10;
