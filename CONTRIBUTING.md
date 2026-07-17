# Contributing

Kigi is a hard fork of [xai-org/grok-build](https://github.com/xai-org/grok-build)
(which does not accept external contributions). Kigi itself welcomes issues
and pull requests.

## Ground rules

- Toolchain is pinned by `rust-toolchain.toml` (Rust 1.97.0, edition 2024).
- Before opening a PR, make sure the CI gates pass locally:

```sh
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets
cargo fmt --all --check
cargo deny check advisories
```

- No telemetry, analytics, or new outbound endpoints. The closed set of
  first-party endpoints lives in `crates/codegen/kigi-env`.
- Upstream syncs are evaluated commit-by-commit and cherry-picked manually;
  see the PRD §8.1 policy (`git range-diff` records the adopted set).
