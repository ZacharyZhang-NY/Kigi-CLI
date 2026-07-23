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
- **Atomic file replace goes through `util::fs::replace_file`** (tmp+rename
  commit step; async callers wrap in `spawn_blocking`). Never inline a bare
  `fs::rename` replace: Windows `MoveFileExW(REPLACE_EXISTING)` fails with a
  sharing violation while AV/indexer/cloud-sync holds the destination open —
  the "persists on macOS, silently doesn't on Windows" class (a /model
  switch that never stuck). Plain rename stays correct only for true moves
  whose destination doesn't pre-exist (worktree-pool markers, corrupt-file
  backups). Write failures must at least `warn!` — never `let _ =`.
- The root `Cargo.toml` is hand-maintained (upstream's generator is not in
  this repo). Members sorted; versions inherited from
  `workspace.package.version` — the single source of truth for the release
  version (`kigi_version::VERSION` derives from it; the release workflow
  gates the `v*` tag against it).

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

- Enabled by default (`KIGI_GRAPH=0` is the off-switch; the G0 gray
  release is over); availability additionally requires the goal harness
  (`BuiltinGate::Graph`).
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
- Every OAuth picker row carries ITS OWN method id (`PendingMenuItem::Login
  { method_id }` → `Action::LoginWith`); an unknown id fails closed. The
  id-less `Action::Login` (auto-login, 401 re-auth) resolves the first
  interactive method. `/login` opens the picker (`Action::OpenLoginPicker`),
  never a flow directly; mid-session its last row is Cancel, not Quit.
- `_meta.connected` on an advertised method is DISPLAY state (green badge on
  the picker): stamped at `initialize()` from stored credentials
  (`connected_method_ids` + `stamp_connected_meta`), kept fresh TUI-side
  after in-session logins (`auth_in_flight_method` → `AuthComplete`). It is
  never an authorization input.
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
      oauth-beta headers. THINKING REPLAY (`prune_replayed_thinking`,
      all Messages requests): Anthropic validates every replayed
      `thinking` block (signature model-bound, non-empty required), so
      only the final assistant message's signed thinking is replayed and
      only while its tool loop is open (request ends on the tool
      results); everything else — unsigned cross-backend history, `tco_*`
      Responses blobs, stale-model blocks — is stripped, or the request
      400s "Invalid `signature` in `thinking` block".
- CROSS-PROVIDER REPLAY POLICY (Pi `transform-messages` pattern): the
  conversation history is provider-agnostic and sessions switch
  models/backends mid-history, so EACH wire builder owns emitting only
  items valid for its target — never patch downstream except in the
  per-backend body adapters. Concretely: the Responses input drops
  Reasoning items without a native `rs_*` id (foreign capture is id "")
  and provenance-gates whole turns via `transform_items_for_responses`
  (`AssistantItem.model_id` vs the request model: foreign Reasoning
  dropped, foreign BackendToolCall demoted to its `text_summary`);
  the codex adapter additionally drops bare `rs_*` references (stateless
  backend); tool-call ids pass through ONE shared ASCII
  `sanitize_tool_call_id` symmetrically on call+result on BOTH the
  Messages and Responses legs; Messages image sources go through
  `parse_base64_image_data_uri` (raster whitelist, no `data:` url
  sources) and empty user turns get a placeholder. Dangling tool calls
  are already repaired item-level by `repair_dangling_tool_calls` on the
  actor's build path. When a provider wire bug surfaces, fix the CLASS
  across all three builders in the same pass — three sequential
  single-provider fixes (thinking signature → codex system role → codex
  reasoning id) motivated this policy.
- ChatCompletions dialect selection: registry platforms declare
  `chat_compat` explicitly; BYOK/custom entries default to `Passthrough`
  (vanilla OpenAI semantics) EXCEPT entries pointed at the house/Kimi
  coding endpoint, which keep the `Kimi` dialect (base-url detection —
  Pi-style quirk sniffing). `ChatCompat::Mistral` = StrictOpenAi plus the
  exactly-nine-`[a-zA-Z0-9]` tool-call id normalizer
  (`normalize_mistral_tool_call_ids`, deterministic FNV-1a→base36, one
  map for call+result; persisted `mistral` values resolve here). Chat
  tool messages are TEXT-ONLY: tool-result images batch into one
  synthetic user message after the consecutive tool-result run
  (`conversation_to_chat_messages`).
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
      is the shared Responses default. BODY adaptation
      (`adapt_body_for_codex_backend`, same gate): the backend 400s
      `role:system` input ("System messages are not allowed") — system items
      are hoisted into the top-level `instructions` field — and stateless
      reasoning replay requires `include:["reasoning.encrypted_content"]`.
      API-key `openai` Responses requests carry NONE of this
      (byte-identical, pinned by a control wire test). `reasoning.effort`
      carries the thinking level (incl. the codex-only `ultra`). NO
      websocket, NO base_instructions.
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
- A stored subscription-OAuth session IS a catalog fetch source. Every
  fetch-PLAN decision that cannot afford real token resolution — the startup
  prefetch arming gate, `on_auth_changed`'s wipe guard
  (`should_wipe_catalog_on_auth_change`), and cache-origin computation — uses
  the sync presence probes `models_fetch::stored_oauth_platforms` /
  `stored_oauth_token_stubs` (auth.json scope scan; names only, no bearers).
  All three must see the SAME enabled-platform set the real fetch enables, or
  a claude-pro-max-only user boots onto the bundled Kimi table with an empty
  picker. Managed catalog entries stamp `meta.provider` (platform display
  name) for the client's `/model` picker; the picker itself lists the fetched
  catalog, which is by construction the credentialed providers' models.
- INFERENCE-AUTH CHOKEPOINT (security): `auth::credential_authority::CredentialAuthority`
  is the ONE authority answering "WHICH credential — if any — may ride this
  request?". It holds the session's effective `EndpointsConfig` and
  its primary `AuthManager` PRIVATELY and answers only
  `(platform, base_url)` questions: `credential_class` → `CredentialClass::{Pooled,
  Primary, None}` (the outer term of `auth_method::session_token_auth_gate`),
  `manager_for` (the governing manager
  for refresh / 401 recovery), `credential_for` (the request's `api_key`) and
  `bearer_resolver_for` (the aux/summary/session resolver). Do NOT re-derive this
  rule anywhere else — three rounds of leaks came from exactly that.
  The rule: a subscription-OAuth platform rides ITS OWN pooled `AuthManager` (so
  it keeps a live `bearer_resolver` and mid-session refresh despite a
  non-first-party base URL) and ONLY at its own `platform.base_url()` — a
  `[model."claude-pro-max/x"]` override keeps `info.id` but can point `base_url`
  anywhere. `kimi-code` and a platform-less model (a bare slug / `[model.*]`
  entry) ride the PRIMARY session, and ONLY at the session's own effective coding
  endpoint: `EndpointsConfig::proxy_url()` (which prefers `[endpoints]
  coding_api_base_url` from **config.toml** — what the managed-config sync writes
  — over `KIGI_CODE_BASE_URL`), `models_base_url`, loopback, or the compiled
  production endpoint. Never a blanket allow: BYOK is `has_own_credentials()`,
  which probes `std::env::var` at call time, so a `[model.*]` block with an unset
  `env_key` classifies `NotByok`. Every API-key registry platform rides NOTHING.
  STRUCTURAL ENFORCEMENT: the authority is the only producer of
  `SessionCredential`, an opaque type with no production constructor, and every
  API that stamps a session bearer onto a request (`resolve_credentials`,
  `resolve_aux_model_sampling_config`, `try_resolve_model_credentials`,
  `resolve_chat_state_auth_type`) takes
  `Option<&SessionCredential>` rather than `Option<&str>` — a new call site
  cannot express the leak. `stamp_session_local_sampler_fields` likewise takes the
  aux `bearer_resolver` explicitly instead of copying the session's and relying on
  the caller to re-point it, and `sampler_turn::aux_bearer_resolver_for` is the
  ONE definition of the aux/summary resolver rule (the session actor and
  `MvpAgent::build_summary_client` both call it; a private second copy is how the
  summary client stayed ungated after M3). NEVER HAND-CARRY A CREDENTIAL TO A
  GUARD (C1): "may **a** session credential ride here" is `true` for
  a subscription-OAuth platform at its own host — where the credential that may
  ride is that platform's POOLED token, never the primary. Ask
  `credential_for(platform, base_url)` and stamp what it returns, so the question
  and the credential are the same object; where you cannot (a credential the
  authority does not own), MATCH on `credential_class` rather than reach for a
  boolean. There is deliberately no second, similarly-named predicate to pick
  wrongly. The shared `sampling_config.api_key`
  (the subagent baseline and the unresolved-model fallback) has exactly TWO
  production writers, both guarded by the authority:
  `MvpAgent::stamp_session_credential` (the `cached_token` / `kimi.com/oidc`
  login handlers and the `new_session` / `load_session` seed), which asks
  `credential_for`; and the `xai.api_key` handler in `acp_agent.rs`, which stamps
  the house `KIGI_API_KEY` read from the environment — a credential the authority
  does not own — only when `credential_class` is `Primary`, the class the house
  key rides and the one every OAuth platform's own host is NOT.
- MODEL→PLATFORM LOOKUP (security): `SamplingConfig::model` is the BARE routing
  slug, and duplicate slugs across platforms are BY DESIGN — an API-key platform
  and its subscription-OAuth twin list identical ids (`xai`/`xai-grok`,
  `anthropic`/`claude-pro-max`, `openai`/`openai-codex`), with the API-key
  platform FIRST in `PlatformId::ALL`. The auth layer therefore resolves the
  platform from the catalog KEY the picker selected, held PER SESSION in
  `SessionActor::selected_catalog_key` (seeded at spawn by
  `agent::models::selected_catalog_key_for_spawn`, rewritten by `SetSessionModel`,
  CLEARED by `OverrideModelName` when the rename makes it stale), via
  `agent::models::entry_for_slug`/`platform_for_slug`.
  NEVER `ModelsManager::current_model_id()`: that cell is process-global,
  last-writer-wins across concurrent sessions, and Leader mode never writes it at
  all (`agent/handlers/model_switch.rs`). The shared `MvpAgent::sampling_config`
  is BUILT from that cell — but ONCE, at startup, and never rebuilt, while the
  cell moves on every non-Leader switch. Its guards therefore read
  `MvpAgent::sampling_config_platform`, the platform captured WITH the config by
  the same `ModelsManager::sampling_config()` call, never a fresh lookup against
  the live cell: once the two drift, re-resolving the config's bare slug falls
  through to `resolve_catalog_key`'s `.rev()` scan, answers the API-key twin, and
  a post-expiry `kigi login` silently leaves the EXPIRED bearer in the config
  that seeds every subagent (H-a). REFUSE RATHER THAN GUESS (H-b):
  when the per-session key does not name the slug, `platform_for_slug` returns
  `None` for a slug that collides across platforms rather than trusting
  `resolve_catalog_key`'s `.rev()` last match — which is the subscription-OAuth
  twin, so the guess hands an API-key session the pooled bearer that REPLACES its
  own key on the wire. `None` then routes purely by the ENDPOINT, which for an
  OAuth host means no credential, no resolver and no adaptation.
  Anything else (aux models, subagent
  overrides) falls back to the picker's own `resolve_catalog_key`, and
  `config::find_model_by_id`'s slug scan takes the LAST match so the two can
  never disagree. Resolving the wrong twin costs the OAuth platform its live
  `bearer_resolver` (unrecoverable 401 ~1h in), its Messages adaptation and its
  Copilot/Codex identity headers — and hands the API-key twin's session a pooled
  OAuth bearer stamped over the user's own key.
- CATALOG VISIBILITY: `platform_wire_model_to_entry` stamps
  `supported_in_api = platform != KimiCode`. `ModelInfo::visible_for_auth`
  reads only the PRIMARY manager's auth mode, so gating the other OAuth
  platforms on it would hide every model from a user who signed in with ONLY a
  Claude Pro/Max, ChatGPT, Copilot, or Grok subscription. Only `kimi-code`
  rides the primary session, so only it may be gated on it.
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
