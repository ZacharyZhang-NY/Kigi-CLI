# Kigi — agent/developer notes

Single source of truth for how this repository is organized and the
constraints every change must respect. Update this file whenever the tech
stack or product direction changes.

## What this is

Kigi is an unofficial Kimi Code CLI community build: a hard fork of
xai-org/grok-build (Apache-2.0, Rust) re-targeted at the Kimi Code
subscription API and the Moonshot open platform. It coexists with the
official `kimi` CLI: binary `kigi`, config dir `~/.kigi`
(`KIGI_SHARE_DIR` override), keyring service `kigi`, env prefix `KIGI_*`.
Never read or write `~/.kimi` (except the explicit one-time read-only
import) or any `KIMI_*` env var.

## Hard constraints

- **Zero egress**: outbound connections are limited to
  `auth.kimi.com`, `api.kimi.com`, `api.moonshot.cn`, `api.moonshot.ai`,
  GitHub Releases domains, and user-configured MCP servers. No telemetry,
  no analytics, ever. `crates/codegen/kigi-env` is the single home of
  first-party endpoints.
- **Toolchain**: Rust 1.97.0 (rust-toolchain.toml), edition 2024.
- **Gates** (all must stay green):
  `cargo check --workspace --all-targets`,
  `cargo clippy --workspace --all-targets` (zero warnings),
  `cargo fmt --all --check`, `cargo deny check advisories`.
- **Observability is local**: `kigi-log` (unified session log, `--debug`
  firehose, subsystem file logs, opt-in instrumentation) writes under
  `~/.kigi` only. Its zero-network property is a contract.
- The root `Cargo.toml` is hand-maintained (upstream's generator is not in
  this repo). Members sorted; versions inherited from
  `workspace.package.version` (0.1.0).

## Layout

- `crates/codegen/` — the bulk of the application (62 `kigi-*` crates).
  Key ones: `kigi-bin` (binary `kigi`), `kigi-tui` (full-screen TUI +
  headless `-p` mode + `acp`/`mcp` commands), `kigi-shell` (agent runtime,
  leader-follower IPC, sessions), `kigi-sampler` (inference client;
  ChatCompletions/Responses/Messages backends), `kigi-auth` (credentials),
  `kigi-config` (config layering, `~/.kigi` paths), `kigi-env` (endpoints),
  `kigi-tools` (tool implementations incl. codex/opencode ports — see its
  THIRD_PARTY_NOTICES.md), `kigi-workspace` (FS/VCS/exec/permissions,
  checkpoint/worktree), `kigi-log` (local observability).
- `crates/common/`, `crates/build/`, `prod/mc/` — shared libs, proto build,
  proxy wire types (the latter to be redefined against Kimi in M1).
- `third_party/` — vendored Mermaid rendering stack (untouched policy).
- `bin/protoc` — dotslash launcher used by proto codegen.

## Test seams

Cross-crate test hooks are behind the `test-support` cargo feature
(kigi-workspace, kigi-pager-render, kigi-config, kigi-tui), enabled via
dependents' `[dev-dependencies]`. Don't expose new test seams as plain
`#[cfg(test)]` items across crate boundaries.

## Milestones (PRD §8.3)

- M0 (done): rename, deletions (voice/telemetry/announcements/marketplace/
  relay-gateway), toolchain, gates.
- M1: Kimi device-flow auth (F1), Moonshot API-key channel (F2), inference
  via ChatCompletions (F3), dynamic model sync (F4). The auth stack in
  `kigi-shell/src/auth` + `kigi-auth` gets rewritten here; transitional
  grok.com references live only there and in `kigi-sampler`/proxy types.
- M2: server-side search/fetch (F5), command parity with kimi-cli 1.49.0
  (F6), one-time `~/.kimi/config.toml` import (F7), F9 smoke list, perf CI.
- M3: GitHub Releases distribution, install scripts, self-update (F8).
