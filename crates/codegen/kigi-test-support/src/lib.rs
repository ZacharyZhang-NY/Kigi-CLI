#![allow(
    unused_imports,
    unused_variables,
    unused_mut,
    unreachable_code,
    dead_code
)]
//! Shared test utilities for kigi crates: mock inference server, SSE
//! generators, ACP stdio clients, headless runner, env sandbox.
//!
//! [`RawStdioClient`] complements [`KigiStdioClient`] for wire bytes the typed
//! client cannot produce (Foundation `\/` methods, string UUID ids).
pub mod acp_client;
pub mod counting_server;
pub mod env;
pub mod headless;
#[cfg(unix)]
pub mod leader;
pub mod mock_server;
mod process;
pub mod scripted;
pub mod sse;
#[cfg(unix)]
pub mod uds_proxy;
pub use acp_client::{KigiStdioClient, RawStdioClient};
pub use counting_server::spawn_counting_server;
pub use env::{EnvGuard, git_workdir, kigi_binary};
pub use headless::{
    HeadlessResult, assert_headless_success, assert_no_crashes, run_headless,
    run_headless_with_cmd, stderr_tail,
};
pub use mock_server::{MockInferenceServer, MockModelEntry, ScriptedResponse, SseEvent};
