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
  GitHub Releases domains, user-configured MCP servers, the endpoints of
  provider platforms the user has credentialed, and `models.dev` (model
  metadata refresh — reached ONLY when an enabled platform's `/models` wire
  lacks metadata, `wire_serves_metadata=false`; Kimi/Moonshot never trigger
  it; `KIGI_MODELS_DEV_URL=0` disables). No telemetry, no analytics, ever.
  `crates/codegen/kigi-env` is the single home of first-party endpoints.
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

## Storage discipline

- Tests that touch the filesystem MUST use `tempfile::TempDir` (drop
  cleans up) — never bare `std::env::temp_dir()` + `create_dir_all`,
  which leaks directories into the OS temp root forever.
- `target/` grows past 150GB across repeated full-workspace builds
  (incremental is already off); run `cargo clean` when it exceeds
  ~50GB and at milestone boundaries.
- Graph node worktrees are removed right after a successful merge-back;
  only FAILED nodes keep theirs for postmortem.

## Test seams

Cross-crate test hooks are behind the `test-support` cargo feature
(kigi-workspace, kigi-pager-render, kigi-config, kigi-tui), enabled via
dependents' `[dev-dependencies]`. Don't expose new test seams as plain
`#[cfg(test)]` items across crate boundaries.

## Graph mode (`/graph`, post-0.1.x — plan.md in the parent dir)

A deterministic DAG scheduler layered over the goal engine: `/graph
<objective>` decomposes the objective into nodes (graph planner subagent
→ Agentproof-style static gate in `graph_plan.rs`), then executes each
node as one ordinary goal — the agentic loop lives INSIDE the node; the
edges stay deterministic Rust. The harness appends a terminal
`gn-final` verification node depending on every planner node.

- Feature flag `KIGI_GRAPH=1` (default off); availability additionally
  requires the goal harness (`BuiltinGate::Graph`).
- Key modules (kigi-shell): `session/graph_tracker.rs` (pure state
  machine; reuses `GoalStatus`/`GoalPhase`/`GoalPauseReason`),
  `session/graph_plan.rs` (planner-JSON contract + validation + fnv id
  canonicalization), `session/graph_planner.rs` (planner runner, reuses
  the goal planner spawn plumbing),
  `session/acp_session_impl/graph.rs` (orchestration seam).
- Seam points: `handle_prompt` intercepts GraphSet/GraphResume; the
  in-turn loop's `EndTurn` arm calls `run_graph_round_end()` to advance
  nodes within the same turn; goal auto-pauses cascade to the graph in
  `auto_pause_goal_if_active_inner`; node goals are armed with the
  REMAINING graph budget so `enforce_goal_token_budget` cascades trips.
- Persistence: `PersistenceMsg::GraphModeState(Option<..>)` →
  `<session_dir>/graph/state.json` (`None` tombstones after clear);
  immutable per-version baselines `graph/graph.baseline.v{N}.json`;
  per-node goal artifacts archived to `graph/<node_id>/`. Restore
  demotes `Active`→`UserPaused` and `Running`→`Ready` (re-run is safe:
  the verifier gates completion).
- `/goal` and `/graph` are mutually exclusive while the graph owns the
  engine; e2e suite: `acp_session_tests/graph/graph_e2e_tests.rs`.
- Parallel fan-out (G1): with `KIGI_GRAPH_CONCURRENCY > 1` (default 3,
  clamp [1,8]) and ≥2 `Ready` nodes, `drive_graph` runs batches via
  `acp_session_impl/graph_workers.rs` — per node a bounded
  worker↔verifier subagent loop (`KIGI_GRAPH_NODE_ROUNDS`, default 3;
  `general-purpose` children; worktree isolation on round 1, resume
  keeps context+worktree on later rounds; `NODE_RESULT:` /
  `NODE_VERDICT:` terminal contracts parsed fail-closed), then
  SEQUENTIAL merge-back via `kigi_workspace::worktree::apply_worktree`
  (`ApplyMode::Merge`); a conflict fails the node and blocks its
  dependents while other chains continue. `gn-final` always runs
  serially on the full goal engine. Concurrency=1 is byte-identical to
  the serial G0 path. Ceiling: a worker round exceeding the foreground
  subagent await budget (600s) is cancelled and retried via resume.
- G2: `BudgetLimited` is resumable — a budget trip demotes in-flight
  nodes to `Ready` (resource stop, not a verdict) and
  `/graph resume --budget <tokens>` re-arms with fresh headroom. The
  pager shows a graph status chip driven by the `GraphUpdated` wire
  variant (`extensions/notification.rs`), emitted from the single
  `persist_graph_state` chokepoint (checkpoint ⇔ badge tick); old
  pagers degrade via `#[serde(other)] Unknown`. PTY scenarios:
  `graph_slash_presession{,_disabled}.yaml`.
- G3: dynamic replan. `DISCOVERED: <text>` line markers (fence-stripped,
  placeholder-filtered) from workers/verifiers/the serial node's final
  text queue as `pending_discoveries`; at each dispatch boundary
  `maybe_replan_graph` (`acp_session_impl/graph_replan.rs`) runs a
  replanner subagent producing an APPEND-ONLY appendix
  (`validate_replan`: existing-id deps allowed, edges onto `gn-final`
  rejected — they would cycle after the final-gating extension),
  bumps `plan_version`, freezes `graph.baseline.v{N}.json`, and regates
  `gn-final` (Ready→Waiting). Bounded by `KIGI_GRAPH_REPLAN_CAP`
  (default 3, 0 = off); past the cap — and after the final node has
  achieved — discoveries drain to history only. Replan failure degrades
  (history + notice); it never pauses a working graph.
- G4: the graph follows the repo. Every checkpoint projects to
  `.kigi/graph.jsonl` at the git root (`session/graph_project.rs`,
  header line + one node per line, atomic write); single writer via an
  fs2 flock sidecar; other instances get read-only `/graph status`.
  Fresh sessions revive via `/graph resume` (load UNDER the lock,
  from_snapshot demotions apply). All lock-then-mutate sites
  identity-check the projected `graph_id`; kigi never commits the file.
- G5: `/graph show` renders box-drawing DAG art
  (`session/graph_render.rs`, Sugiyama-lite: longest-path layers, dummy
  pass-throughs, barycenter ordering, bus lanes). Wider than 120 cols
  degrades to the status tree.
- G6: plan-boundary topology optimizer
  (`acp_session_impl/graph_optimize.rs`; `KIGI_GRAPH_OPTIMIZER=0`
  disables). Restricted ops (`remove_dep`/`reorder`/`merge`/`split`)
  validated by `graph_plan::apply_optimization`: pending-only targets,
  immutable nodes byte-identical in the result, terminal gate rebuilt,
  whole-graph acyclicity. Applied passes bump `plan_version` and share
  the replan cap; `{"ops": []}` is a respected free no-op; failures
  degrade.

## Provider registry & API-key auth (post-0.1.3 expansion)

- The platform registry is compiled-in spec rows in `kigi-models`
  (`PlatformSpec`; adding a platform = enum variant + `ALL` entry + `spec()`
  arm + row; registry tests enforce completeness/uniqueness/row shape).
- API-key resolution precedence, per platform: platform env var(s) >
  `auth.json` scope named by the platform id (`moonshot-cn`, …) >
  legacy `[platforms.<id>]` in config.toml (read-only fallback).
- The TUI login picker persists pasted keys to `auth.json` (platform-id
  scope, `api_key` mode) — never to config.toml. The keyring holds ONLY the
  OAuth session scope; platform keys are file-only.
- Auth method ids over ACP equal the platform ids; interactive picker rows
  are built generically from advertised methods (`AuthMethodKind::
  ApiKeyPlatform`), so new registry rows appear in the picker with no TUI
  changes.
- Refreshable-OAuth providers beyond Kimi Code use a GENERIC path, NOT Kimi's
  bespoke wire. A `uses_oauth` platform carrying `oauth: Some(&OAuthConfig)`
  (client id / auth host / start+token paths / `token_host` / `scope` /
  `scope_key` / optional extra device field / `flow` / `token_body`) drives a
  scope-keyed `AuthManager::new_oauth_provider` +
  `refresh::GenericDeviceRefresher` (selected by `build_refresher` via
  `oauth_config_for_scope_key`; the refresher dispatches the refresh body by
  `token_body`: form → `auth::oauth_device`, JSON → `auth::oauth_pkce`,
  `GithubCopilotExchange` → `auth::github_copilot` copilot-token re-mint). Kimi
  Code keeps `oauth: None` and its bespoke path unchanged. The interactive
  login is dispatched by `OAuthConfig.flow` (in `run_oauth_provider_flow`):
  - `OAuthFlow::DeviceCode` → `auth::oauth_device` (RFC-8628 device-code, plain
    kigi UA, no X-Msh headers). Provider: `xai-grok` (`scope_key oauth/xai`,
    base `api.x.ai/v1`, form token body, same wire as the API-key `xai` row).
  - `OAuthFlow::PkceLocalhost { redirect_port, redirect_path }` →
    `auth::oauth_pkce` (authorization-code + PKCE S256,
    `127.0.0.1:redirect_port{redirect_path}` loopback with STRICT `state`
    validation + manual-paste fallback, authorize host possibly ≠ token host).
    The `token_body` selects the login dialect: `Json` = claude (`state ==
    verifier`, JSON exchange carrying `state`), `Form` = codex (fresh-random
    `state`, FORM exchange without `state` via `exchange_code_form`).
    `OAuthConfig.authorize_extra` appends provider-only authorize params (empty
    for all but codex). Providers:
    - `claude-pro-max` (`scope_key oauth/claude-pro-max`, port 53692
      `/callback`, JSON body, base `api.anthropic.com/v1`, Anthropic Messages +
      listing wire reached with an OAuth `sk-ant-oat…` Bearer). Its Messages
      requests take the OAuth adaptation — `anthropic-beta claude-code-…,oauth-…`
      + `claude-cli` UA + `x-app cli` + the required "You are Claude Code…"
      system prefix — gated on `SamplerConfig.anthropic_oauth` (claude-pro-max
      only), so API-key `anthropic`/`minimax` Messages requests stay
      byte-identical. Its `/v1/models` listing rides the same Bearer +
      oauth-beta headers.
    - `openai-codex` (ChatGPT Plus/Pro, `scope_key oauth/openai-codex`, port
      1455 `/auth/callback`, FORM body, authorize+token host `auth.openai.com`,
      client `app_EMoam…`, scope `openid profile email offline_access`, the 3
      authorize-extra params `id_token_add_organizations`/
      `codex_cli_simplified_flow`/`originator=codex_cli_rs`). Refresh is a plain
      `refresh_token` FORM grant (the generic refresher's `Form` path →
      `auth::oauth_device`). The minted `access_token` is a JWT; login FAILS
      FAST unless it carries the `["https://api.openai.com/auth"]
      ["chatgpt_account_id"]` claim (`chatgpt_account_id_from_jwt`) — that
      account id is NOT persisted but re-derived STATELESSLY from the current
      bearer at every request. INFERENCE reuses the EXISTING Responses wire
      against base `chatgpt.com/backend-api/codex` (→ `{base}/responses`) with
      a Codex-gated adaptation (`SamplerConfig.openai_codex` /
      `PlatformId::sends_codex_responses_headers()`): headers
      `chatgpt-account-id` (per-request from the JWT), `originator codex_cli_rs`,
      `OpenAI-Beta responses=experimental`, a codex `User-Agent`; `store:false`
      is the shared Responses default. API-key `openai` Responses requests carry
      NONE of this (byte-identical). `reasoning.effort` carries the thinking
      level (incl. the codex-only `ultra`). NO websocket, NO base_instructions.
      CATALOG is HARDCODED (`PlatformId::hardcoded_catalog` →
      `openai_codex_wire_models`, mapped through the SAME
      `platform_wire_model_to_entry` output): exactly the 4 `visibility=list` &&
      `supported_in_api=true` models (`gpt-5.6-sol/terra/luna`, `gpt-5.5`, ctx
      272000, per-model efforts) — NO live `/models` fetch, NO codex-CLI /
      `~/.codex` dependency; `gpt-5.3-codex-spark` (api=false) and
      `gpt-5.4`/`gpt-5.4-mini`/`codex-auto-review` (hidden) are EXCLUDED.
  - `OAuthFlow::GithubDeviceCopilot` → `auth::github_copilot` (TWO-STAGE).
    Provider: `github-copilot` (`scope_key oauth/github-copilot`, base
    `api.individual.githubcopilot.com`, ChatCompletions wire). Stage 1 is an
    RFC-8628 device flow on `github.com` (client `Iv1.b507a08c87ecfe98`, scope
    `read:user`) whose errors ride a `200` body (not `4xx`) — it mints the
    DURABLE github token. Stage 2 (`GET api.github.com/copilot_internal/v2/token`
    with `copilot_exchange` + editor headers) re-mints the SHORT-LIVED copilot
    token. Persisted as `KimiAuth.key = copilot token`, `refresh_token = github
    token`, `expires_at = copilot expiry`; the "refresh" is a copilot-token
    RE-MINT (GET, not a `refresh_token` grant). Every `/models` listing AND
    `/chat/completions` request carries the VS Code editor-identity headers
    (`User-Agent GitHubCopilotChat/…`, `Editor-Version`, `Editor-Plugin-Version`,
    `Copilot-Integration-Id`; `+X-GitHub-Api-Version` on `/models`, `+X-Initiator
    user` on inference) — gated on `SamplerConfig.github_copilot` /
    `PlatformId::sends_copilot_editor_headers()` so every other ChatCompletions
    provider stays byte-identical. WIRE-COMPAT SCOPE: Kigi is one-wire-per-
    platform, so the catalog is FILTERED (`parse_github_copilot_listing`) to the
    openai-completions-served models — keep iff `model_picker_enabled` &&
    `policy.state != "disabled"` && `tool_calls != false` AND the id is NOT a
    `claude-(haiku|sonnet|opus)-[45]` (anthropic-messages) or `gpt-5/oswe/mai-`
    (responses-only) model. Those excluded models need per-model wire routing
    (deferred, documented debt), NOT included lest they fail at inference.
    KNOWN LIMITATION: Kigi does NOT port Pi's per-model policy-acceptance step
    (`POST {base}/models/{id}/policy {state:"enabled"}`). A kept model whose
    Copilot policy is unconfigured can list yet `403` at inference until the user
    enables it once in GitHub's UI — a deliberate omission (it mutates account
    state and is unverifiable without a live Copilot account), not a silent gap.
  These are INTERACTIVE login rows advertised right after `kimi-code`
  (`AuthMethodKind::OAuthPlatform`, in `PlatformId::ALL` order: `xai-grok`,
  `claude-pro-max`, `github-copilot`, `openai-codex`). The catalog fetch resolves each such platform's OWN session
  token (`resolve_generic_oauth_tokens`, refreshed on expiry) and routes
  `platform.oauth().is_some()` → `platform.base_url()` (kimi-code alone →
  `proxy_url()`). Tokens/codes/verifiers are NEVER logged.
- Model metadata (context window, thinking levels) comes from the provider
  wire when served; metadata-poor listings are enriched from models.dev
  (`kigi-models/src/enrichment.rs` — bundled raw snapshot regenerated by
  `scripts/gen_enrichment_snapshot.py`, single Rust transform
  `parse_api_json` for bundled + runtime refresh; 24h cache
  `~/.kigi/models_dev_cache.json`). Wire values always win; enrichment
  never invents model availability. Canonical reasoning efforts:
  none/minimal/low/medium/high/xhigh/max/ultra (`max` split from `xhigh`
  2026-07; `ultra` is codex-only, above `max`, surfaced only via a model's
  server-declared effort menu; Kimi wire spells its top tier `max`, kimi_compat
  renames).

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
