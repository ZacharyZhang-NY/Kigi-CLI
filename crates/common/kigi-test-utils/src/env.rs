//! Environment-variable test knobs.

/// Parse a `usize` env knob; use `default` when unset or unparseable.
/// Perf-repro convention for sizing `#[ignore]` benches (e.g. `KIGI_PERF_GIT_FILES`).
pub fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
