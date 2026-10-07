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

Upstream sync record (update on every sync): fork baseline is upstream
snapshot `8adf901` (2026-07-16, ≈v0.2.102); bug fixes evaluated and
selectively ported through upstream `77cd7eb` (2026-08-25, ≈v1.0.10) in
the 0.1.13 cycle, through `3794978` (2026-09-09, ≈v1.0.24) in the 0.1.15
cycle, through `a28ee2b` (2026-09-17, ≈v1.0.35) in the 0.1.16 cycle, and
through `2bdd1d6` (2026-09-29, ≈v1.0.45) in the 0.1.19 cycle.
The local release is 0.1.20; it adds custom providers declared by base
URL (`[platforms.<name>]`, see CUSTOM PROVIDERS below).
Every upstream reference comes from the local clone at `./grok-build`
(gitignored, never committed): `git -C grok-build pull` before a sync,
then read upstream code and history there, not from the web.
Upstream is daily "Synced from monorepo" snapshots —
the commit BODIES carry per-change bullet lists, and
`crates/codegen/xai-grok-shell/changelogs/` maps releases to dates;
start any future sync from those two, not from raw diffs.

Known limitations left by the 0.1.15 sync (fix before relying on them):
the Windows frame writer writes through `std::io::stderr`, so a console
that stops reading still blocks exit-path prints behind std's stderr lock
(unix silences fd 2 after a timed-out writer join); the bwrap command
builder re-checks `Path::exists("/data")` and can omit the devbox bind on
a stat error that the profile classification treated as present.

Known limitation left by the 0.1.16 sync: `notifications/cancelled` for a
timed-out MCP call cannot reach a streamable-HTTP server that answers
`tools/call` with a plain JSON body. rmcp 2.2.0's HTTP worker awaits each
POST in turn, so the cancel queues behind the hung call;
`servers/cancel_aware.rs` waits 500 ms, warns, and the transport reset
then drops the connection. stdio and SSE-mode servers get the cancel.
The cure is rmcp 3.x (upstream runs 3.2), a major bump not yet taken.

Evaluated in the 0.1.19 window and left for a later cycle: headless `-p`
interrupt handling (upstream `SignalStreams` plus a headless signals module; Kigi's
`headless.rs` installs no handler, so SIGINT kills the run without session
cleanup), the subagent "Cancelling" overlay that outlives the child turn (needs
upstream's child-turn finalizer and its prompt-id bookkeeping), the honest
answer for a `spawn_subagent` call cancelled by send-now (a chat-state answers
channel), the effort picker preselecting the model's default effort and
accepting menu labels (tied to upstream's `ModelChoice` refactor), and every MCP
change that rides rmcp 3.x (`bearer_token_file`, argument coercion, elicitation
notes). The rmcp and tree-sitter major bumps stay open.

Evaluated in the 0.1.16 window and left for a later cycle: MCP qualified
tool names past 64 chars (first prove the name never reaches a provider
function list), the auto-update guard for a binary still in use (needs
per-OS process scans), the DA1 fence around the kitty keyboard pop (needs
a design against the 0.1.15 teardown), MCP servers on protocol 2026-07-28
(rmcp 3.x again), and `pager.toml`, whose writer in
`kigi-pager-render/src/appearance/config.rs` still ends in a bare
`fs::rename` and replaces a symlink.

## Hard constraints

- **Zero egress**: only `auth.kimi.com`, `api.kimi.com`, `api.moonshot.cn`,
  `api.moonshot.ai`, GitHub Releases, user-configured MCP servers, credentialed
  provider endpoints, and `models.dev` (metadata refresh, ONLY when an enabled
  platform's `/models` lacks it — `wire_serves_metadata=false`, never
  Kimi/Moonshot, `KIGI_MODELS_DEV_URL=0` disables). No telemetry, no analytics,
  ever. `crates/codegen/kigi-env` is the single home of first-party endpoints.
- **One SSRF policy**: `ssrf::is_blocked_for_host` gates every model/settings
  URL (`web_fetch` + hooks), per redirect hop. Never write a second copy.
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
- **config.toml writers resolve their destination through
  `util::config::config_write_dest` before they name the tmp file.** A
  symlinked `~/.kigi/config.toml` (dotfiles) keeps its link: the tmp sits
  beside the referent and replaces it. Only that one path is followed, and
  only while `user_kigi_home()` resolves (with no home, `kigi_home()` is the
  project's own `./.kigi`). A project `.kigi/config.toml` is
  repo-controlled, and following its symlink would overwrite whatever file
  a hostile repo points it at; there the link itself is replaced.
  Credential and managed stores never follow symlinks.
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

## Terminal keyboard

`Shift+Enter` inserts a newline in the prompt wherever the terminal can
report the modifier, and `kitty_skip_reason` decides where that is —
`shift_enter_unavailable` matches on it rather than keeping a second brand
list, because the two drifted once and the UI advertised a chord that never
arrived. Two skips still deliver the modifier: Apple Terminal, where
`route_enter` polls CoreGraphics, and Windows Terminal, where the console
key record carries SHIFT with no protocol at all (a bare ConHost is only
optimistically refined to Windows Terminal, so it keeps advertising
Alt+Enter). Everywhere else — VTE, xterm.js, JediTerm, screen, tmux below
3.3, tmux with `extended-keys off` — the byte on the wire is a plain CR and
no client-side trick recovers it: crossterm 0.28 parses kitty `CSI 13;2u`
but not xterm's `CSI 27;2;13~`, so `modifyOtherKeys` buys nothing. NO VTE
release carries the protocol (checked at tag 0.82.0:
`_vte_keymap_GDK_Return` encodes only Alt); a version cutoff for it is a
bug, not a feature. `/terminal-setup` owns the per-terminal recipe.

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

## Swarm (`agent_swarm` tool + `/swarm` mode)

Cheap, unstructured fan-out — the one thing `/graph` does not offer. One
`prompt_template` containing `{{item}}` is expanded over an `items` list
into up to `MAX_AGENT_SWARM_MEMBERS` (128) ordinary subagents, run to
completion, and returned as ONE aggregate. Ported from kimi-code; entirely
client-side, no backend surface.

- Engine (`kigi-tools/.../kigi/agent_swarm/`): `plan.rs` validates and
  expands (every fault is reported BEFORE a member starts; expanded
  prompts must be pairwise distinct), `schedule.rs` is the launch ramp as
  pure arithmetic (5 immediate, then 1 per 700ms; capacity shrinks on a
  provider rate limit and recovers after a quiet window;
  `KIGI_AGENT_SWARM_MAX_CONCURRENCY` caps it and a malformed value is a
  hard error), `run.rs` drives it against the EXISTING single-spawn
  `SubagentBackend`, `tool.rs` is the tool.
- `SwarmMemberOutcome::Backgrounded` is load-bearing: a member that
  outlives the 600s foreground budget is detached by the coordinator and
  KEEPS RUNNING. It is never reported `Failed` and never offered for
  resume — relaunching its item would put a second agent on the same
  files. Members share the caller's tree with NO isolation; the
  distinct-prompt rule is the only thing keeping them apart.
- `InFlightGuard` (run.rs) cancels live members when the runner's future
  is dropped. Send-now cancels the turn WITHOUT cancelling subagents and
  aborts the task; the dropped receivers read as "parent gone" and each
  child re-attaches itself. There is no cooperative path to use instead —
  `kigi_tool_runtime::Cancellation` is constructed nowhere.
- Both retries (`MAX_RATE_LIMIT_RETRIES`) and wall clock
  (`MAX_SWARM_RUNTIME`) are bounded: the swarm blocks the caller's turn,
  so every wait needs a ceiling. Stragglers at the deadline are reported
  as still-running WITH their ids.
- Underneath every fan-out (swarm, graph, raw Task) sits the
  session-scoped spawn bound: `handle_subagent_request` holds one permit
  from the parent session's semaphore for the whole child run
  (kigi-tools `task/admission`: 32 default via
  `KIGI_MAX_CONCURRENT_SUBAGENTS`, queue|fail via
  `KIGI_SUBAGENT_LIMIT_BEHAVIOR`), so wide fan-outs queue instead of
  exhausting file descriptors.
- `ToolKind::AgentSwarm` is its own variant because `TemplateRenderer`'s
  `by_kind` map holds ONE tool name per kind — sharing `Task` would
  silently redirect `${{ tools.by_kind.task }}` in other tools' prompts.
  It gates exactly as `Task` does (`capability.rs`), and `builder.rs`
  strips it wherever `task` is stripped. `MAX_SUBAGENT_DEPTH` stays 1:
  upstream's unlimited nesting is a hazard, not a feature.
- `/swarm | /swarm on|off | /swarm <task>` (`BuiltinGate::Swarm` = the
  tool is in the toolset) arms a doctrine reminder via the existing
  `push_system_reminder`. Two triggers only: `Manual` persists until
  switched off, `Task` auto-expires at turn end via `SwarmTurnGuard` — a
  DROP guard, because a user interrupt aborts the turn future and never
  reaches post-loop code. `enter` is a total no-op while armed so the
  `/swarm <task>` shorthand cannot downgrade a standing `/swarm on`.
- The mode is deliberately NOT persisted (unlike `/goal` and `/graph`,
  which strand real work if lost): it is a prompt hint whose recovery is
  retyping one command. The injected doctrine IS durable, so an explicit
  `/swarm off` retracts UNCONDITIONALLY — a resumed or compacted session
  can read as "off" with the instruction still in context.

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
      carries the thinking level. NO websocket, NO base_instructions.
      CATALOG is HARDCODED (`PlatformId::hardcoded_catalog` →
      `openai_codex_wire_models`, mapped through the SAME
      `platform_wire_model_to_entry` output): the 8 `visibility=list` &&
      `supported_in_api=true` models the backend served on 2026-09-29
      (`gpt-6.1-sol`, `gpt-6-astra/sol/luna`, `gpt-5.6-sol/terra/luna`,
      `gpt-5.5`, ctx 272000, per-model efforts) — NO live `/models` fetch, NO codex-CLI / `~/.codex`
      dependency; the hidden `gpt-reserve` and `codex-auto-review` are
      EXCLUDED. Refresh the table by reading
      `GET {base}/models?client_version=<a current codex release>` with a
      ChatGPT bearer: an old `client_version` is answered with a truncated
      list, so a stale version reads as "the model does not exist".
      `CODEX_USER_AGENT` (`codex_cli_rs/0.159.0`) moves with the release used
      for that read.
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
  - `OAuthFlow::DevinPkce` → `auth::devin` (native Connect/protobuf provider).
    Provider: `devin` (`scope_key oauth/devin`, inference base
    `https://server.codeium.com`, `KIGI_DEVIN_BASE_URL` override,
    `PlatformWireApi::Devin` / `ApiBackend::Devin` /
    `ListingDialect::Devin`). LOGIN: PKCE S256 with an independent random
    `state`, loopback `127.0.0.1:59653/callback` bound BEFORE the authorize
    URL is published (ephemeral port only on AddrInUse or in tests); the
    authorize query is EXACTLY `redirect_uri,state,prompt=select_account,
    code_challenge,code_challenge_method` — the dialect carries no
    client_id/response_type/scope. The manual-paste path accepts ONLY this
    login's exact callback URL (`parse_devin_callback`: matching
    scheme/host/port/path, no userinfo/fragment, exactly one `code` plus the
    matching `state`; bare codes and `code#state` are rejected). Exchange
    POSTs JSON `{code,code_verifier}`
    to `api.devin.ai/auth/cli/token` for `{token}`: a SESSION token, NOT a
    refresh token — `OAuthTokenBody::DevinSession` makes the generic
    refresher return permanent failure with ZERO token-host HTTP (no fake
    refresh); expiry/revocation means re-login. Opaque tokens carry no server
    TTL (the 30-day Kigi fallback applies); JWT `exp` is honored when present.
    WIRE (`kigi_sampling_types::devin`, pure, no I/O): minimal prost structs
    over the `exa.*` schemas. Auth NEVER rides an HTTP header — the session
    token is normalized once to `devin-session-token$…` and travels inside
    `Metadata.api_key` (`disable_telemetry` always true; released-CLI identity
    `devin-cli/chisel` `3000.11.3` for chat/auth, `chisel` `0.0.0-dev` for
    discovery). Unary `GetUserJwt` mints a per-request user JWT (a
    `custom_api_server_url` that differs from the configured base fails
    CLOSED — configure the endpoint explicitly; enterprise reroute is
    deferred), then `GetChatMessage` streams a Connect frame sequence
    (`application/connect+proto`, flags 0x01 gzip / 0x02 end-stream) through a
    redirect-NONE client with 16 MiB bounds on both wire and decompressed
    payloads; an oversize advertisement fails before any payload byte is
    staged, and bytes after the end trailer are a hard error, never success.
    CATALOG: unary `GetCliModelConfigs` → `ClientModelConfig` rows
    (nested `ModelInfo` wins context/capabilities; disabled, blank-uid,
    router, and features-declared-no-tool-calls rows excluded; each concrete
    UID stays separately selectable — no AssignModel, no effort collapsing).
    FAMILIES ride `ClientModelConfig.model_family_metadata` (tag 30),
    `is_default_model_in_family` (tag 31), and `ModelInfo.model_family_uid`
    (tag 23) → `WireModel.model_family` → stored `ModelEntryConfig` /
    `ModelInfo` → ACP `_meta.modelFamily` (`{id,name,isDefault}`; the TUI
    consumes `kigi_shell::agent::config::ModelFamilyInfo`, re-exported, and
    trusts the meta only on `devin/`-prefixed ids). The `/model` picker and
    `kigi models` collapse multi-member families to one header row; family
    name / `devin/<family.id>` / `devin/<family.name>` aliases resolve the
    flagged default variant (first member when unflagged), while exact
    concrete ids and full variant names always win. The Devin component of
    the models disk-cache origin is suffixed `#model-families-fusion-v3` so
    cached catalogs from before a catalog-shape change refetch. ROUTER rows
    (the `fusion-…` pairs, `adaptive`) are served ONLY when the discovery
    `Metadata` carries tag 30 = packed `[3,4,6,7,8]`, read off the released
    CLI's own request on 2026-10-06; without it the server answers the 260
    concrete models and `/fusion` stays hidden. FUSION rows (`fusion-<lead
    uid>-sidekick-<helper uid>` routers) project in a second pass against
    the eligible-concrete index — a pair row only materializes when BOTH
    component uids survive the ordinary filters (a `-fast` lead segment
    resolves the `-priority` concrete, the GPT speed-variant spelling) (never synthesized, never
    disabled/malformed); it clones the lead's capabilities/context and
    carries `WireModel.fusion` (`{lead,sidekick,leadModel,sidekickModel}`
    → `_meta.fusion`). At sampling time a fusion uid runs
    `AssignModel` (tag2 router uid, tag3 cascade id, tag5 latest USER
    prompt) AFTER `GetUserJwt` — the returned `assignment_jwt` rides
    `GetChatMessageRequest` tag 26 and the returned `model_uid` becomes
    the chat uid (missing/blank/echoed-router assignments fail 502 with
    no chat; JWTs are per-request, never persisted). A fixed Fusion
    system paragraph is appended to the chat `prompt` naming the raw
    lead/helper uids. Subagent routing: an unpinned child under a
    `devin/` fusion parent resolves `devin/<sidekick_model>` through the
    normal override path (existing pins/fork_context win; stale slug
    guard `entry.info.model == live cfg.model` gates the lookup). TUI:
    `/fusion` groups native pairs (meta `leadModel`/`sidekickModel`)
    under one row per lead — Enter expands sidekicks — while `/model`
    hides pair rows (exact `devin/fusion-…` ids still resolve);
    `kigi models` lists them under the `Fusion (Devin)` family. The
    helper-side loop is Kigi-local — it does not reproduce Devin's
    sidekick prompts; the real helper dispatch shape is not confirmed by
    the bounded mock probe (it may be feature-gated or fixture-artifact).
    STREAMING: `DevinEventTranslator` emits Anthropic-Messages events so the
    existing `stream_messages` L2 keeps reasoning/tool/usage/cancel/idle
    semantics; `message_stop` only after a valid end trailer (error trailers
    map the fixed Connect code vocabulary to status — anything else reads
    `unrecognized`/502; EOF is never success). Usage buckets:
    tag2 input(uncached), 3 output, 4 cache-write, 5 cache-read. REPLAYED
    THINKING uses a `kigi-devin-v1:` JSON provenance envelope
    (`pack_devin_signature`) — replayed only for a same-model final
    open-tool-loop assistant; `prune_replayed_thinking` strips it from the
    Messages wire, and the Responses builder drops it via its existing
    foreign-item (empty-id) check. Out of scope for v1: hosted tools and
    native JSON-schema responses (the StructuredOutput tool path covers it).
  These are INTERACTIVE login rows advertised right after `kimi-code`
  (`AuthMethodKind::OAuthPlatform`, in `PlatformId::ALL` order: `xai-grok`,
  `claude-pro-max`, `github-copilot`, `openai-codex`, `devin`). The catalog fetch resolves each such platform's OWN session
  token (`resolve_generic_oauth_tokens`, refreshed on expiry) and routes
  `platform.oauth().is_some()` → `platform.base_url()` (kimi-code alone →
  `proxy_url()`). Tokens/codes/verifiers are NEVER logged.
- CUSTOM PROVIDERS (0.1.19): `[platforms.<name>]` with `base_url` and `api =
  "openai" | "anthropic"` declares a provider outside the closed registry.
  `kigi-models/src/custom.rs` owns the validated type (name `[a-z0-9][a-z0-9_-]{0,31}`,
  never a registry id; base URL `http(s)://host[/path]`, no userinfo, query or
  fragment); `kigi-shell/src/agent/custom_providers/` owns the rest. Key order:
  auth.json scope `<name>`, then the table's `api_key`; a key is REQUIRED (a
  keyless declaration is inert). Wires: openai = `/chat/completions`, Bearer,
  `GET {base}/models`; anthropic = `/messages`, `x-api-key`,
  `GET {base}/models?limit=1000`. Catalog keys are `<name>/<model>`; the entry's
  `description` carries the provider name for the picker. Entries reach the disk
  cache without a key; `stamp_credentials` stamps `api_key` in memory, only onto
  entries still on the provider's own host, and `drop_orphans` removes fetched
  entries of a provider that is gone or keyless BEFORE the prefetched map replaces
  the catalog (a stale unkeyed entry would fall through to the house key).
  CREDENTIAL RULE: `EndpointsConfig.custom_provider_bases` (derived from
  `[platforms.*]` in `Config::new_from_toml_cfg` and `from_config_value`, never
  serialized) makes `CredentialAuthority::is_session_coding_endpoint` false for a
  declared base, loopback included — without it an Ollama-style provider on
  localhost classifies `Primary` and the Kimi bearer replaces its key. Every
  authority is built from the models manager's config, so the field reaches all of
  them. Turns and subagents read `auth_scheme` and BYOK status from the LIVE
  catalog entry (`auth_facts_of`), not from a disk reload: fetched entries never
  live on disk, and the reload sent fetched Messages models out with `Bearer`
  instead of `x-api-key` (this also repaired `anthropic` and `minimax`).
  Login: picker rows `custom-openai` / `custom-anthropic` are not loginable
  (`AuthMethodKind::Unknown`); the TUI asks base URL, name (default from the
  host), key, then opens a MODEL-SELECTION screen (`custom_entry.rs`
  `CustomSelectState`, welcome `AuthMode::CustomProviderSelect`): the listing
  fetch (`fetch_listing_for_login`) runs concurrently with the manual-id input
  — both visible at once, failure shows its reason — space toggles fetched
  rows, enter adds a typed id or finishes (a finish pressed mid-fetch waits for
  it: `finish_requested`), and the chosen ids persist as `[platforms.<name>]
  models = [...]` via `save_custom_provider`. The listing fetch probes
  `{base}/v1` once when `{base}/models` 404s or answers unparsable (a WAF
  challenge is HTTP 200 HTML) and the base has no `/vN` tail, adopting the
  probed base for the saved definition; anthropic-wire listings that 401 on
  `x-api-key` retry once with `Bearer` (relays gate `GET /models` on Bearer
  while `/messages` takes `x-api-key`); listing errors never quote response
  content (line/column only — a reflected body could carry the key). Catalog
  assembly honors the allowlist in `selection.rs`: `synthesize_selected` (BEFORE
  `[model.*]` overrides, so an override composes with a synthesized entry)
  builds wire-default entries for selected ids the listing never served, and
  `filter_unselected` (AFTER overrides) is authoritative in the provider's
  namespace — even a `[model."name/x"]` entry for an unselected id is dropped.
  Provider-owned entries never take donor `api_backend` inheritance in
  `resolve_model_list` (the declared wire wins); no `models` key = the whole
  listing (the declarative path). `authenticate("custom:<name>")` then
  validates `GET {base}/models` (401/403 = rejected key, other non-2xx and network
  errors surface). Limits: context window 256K unless the listing serves
  `context_length` (Anthropic: `max_input_tokens`); no models.dev enrichment; a
  failed key check leaves the saved definition (login again with the same name to
  overwrite); no delete UI, edit config.toml and auth.json.
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
  none/minimal/low/medium/high/xhigh/max (`max` split from `xhigh` 2026-07;
  the Codex backend advertises an `ultra` tier above `max` that kigi has no
  level for; Kimi wire spells its top tier `max`, kimi_compat renames).
  config.toml accepts any letter case: `ReasoningEffort` deserializes
  through `FromStr`.

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

## Native Devin release smoke (5e4da82)

The Linux x86_64 `v0.1.20-native-devin.1` release binary was exercised
against an isolated loopback Connect/protobuf mock, without real account
credentials. The workspace compiles clean: `cargo check --workspace
--all-targets` and `cargo clippy --workspace --all-targets` pass with zero
warnings. Catalog filtering and the explicitly authenticated
ACP `devin` path completed a local `read_file` tool loop and streamed a final
answer. The recorded assertions above remain mock-based; separately, the user
has confirmed real Devin login and direct inference work interactively.

Known issues observed in this release:
- A profile containing only `oauth/devin` can discover its model catalog, but
  headless `-p` exits with the Kimi-oriented "Not signed in" error before any
  inference RPC. The eager/default authentication gate does not adopt the
  provider's cached session.
- The standalone `kigi login` command still runs Kimi Code login, not the
  provider picker. Native Devin login is exposed by the built-in Kigi agent's
  login picker / `/login`; do not describe `kigi login` as a Devin picker.
- A session-title auxiliary request carried `kimi-for-coding` as its Devin
  model UID even though that UID was absent from the mock account catalog.
  Production acceptance of that UID has not been verified.
- `kigi models` displays "You are not authenticated" with this Devin-only
  profile even while successfully listing the authenticated Devin catalog.
- A chat stream missing its Connect end trailer was retried and did not return
  a final prompt result within the 45-second smoke-test window. No successful
  completion was observed, but a bounded failure was not established.
