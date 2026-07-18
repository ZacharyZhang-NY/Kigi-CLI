# Smoke checklist (PRD F9)

F9 carries the grok-build harness capabilities forward unchanged. Each row
names the capability, the automated case that covers it (run with
`CARGO_INCREMENTAL=0`), and the manual probe when automation cannot reach it.
A release is smoke-clean when every row has at least one passing case.

Legend: `auto` = covered by the named test target in CI; `manual` = run the
listed command and check the listed observable.

| # | Capability | Case | How |
|---|------------|------|-----|
| 1 | Fullscreen TUI (mouse, themes, shortcuts, slash commands) | auto | `cargo test -p kigi-tui --lib` (welcome/menu/mouse/theme suites, 6.6k tests); manual spot: `kigi` → welcome moon renders, `ctrl+q` quits |
| 2 | Headless / script mode | auto | `cargo test -p kigi-tui --lib headless`; manual: `kigi -p "Reply OK"` prints the reply and exits 0 |
| 3 | ACP server | manual | `printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}}\n' \| kigi acp` → one JSON-RPC result line with `"protocolVersion":1` |
| 4 | MCP client (stdio/http/oauth) | auto | `cargo test -p kigi-mcp --lib`, `cargo test -p kigi-shell --lib util::config::mcp`; manual: `kigi mcp add t -- npx -y @modelcontextprotocol/server-everything` then `kigi mcp doctor t` |
| 5 | `--mcp-config-file` injection | auto | `cargo test -p kigi-shell --lib util::config::mcp::tests::cli_mcp`; manual: flag + `x.ai/mcp/servers_updated` notification lists the injected server |
| 6 | Skills | auto | `cargo test -p kigi-agent --lib skills` |
| 7 | Plugins (local load) | auto | `cargo test -p kigi-agent --lib plugins`; manual: `kigi plugin list` |
| 8 | Hooks | auto | `cargo test -p kigi-hooks --lib` and `cargo test -p kigi-shell --lib hooks` |
| 9 | Sandbox | auto | `cargo test -p kigi-sandbox --lib` |
| 10 | Checkpoint / session persistence | auto | `cargo test -p kigi-shell --lib session::` (persistence/rewind suites); manual: `kigi sessions` lists the last session, `kigi -c` resumes it |
| 11 | Worktrees | auto | `cargo test -p kigi-fast-worktree --lib`; manual: `kigi worktree list` |
| 12 | Mermaid rendering | auto | `cargo test -p kigi-mermaid --lib` |
| 13 | Crash handling | auto | `cargo test -p kigi-crash-handler --lib` |
| 14 | Kimi auth (device flow) | auto | `cargo test -p kigi-shell --lib auth::` (138 cases incl. live-shape fixtures); manual: `kigi login` completes in a browser, token lands in keyring service `kigi` |
| 15 | Inference (ChatCompletions Kimi dialect) | auto | `cargo test -p kigi-sampler` (incl. `test_kimi_wire`); e2e: `scratchpad` mock flow (write → run → answer), see AGENTS.md |
| 16 | Model catalog sync (`/models`) | auto | `cargo test -p kigi-shell --lib models_fetch` + `cargo test -p kigi-models --lib` |
| 17 | Search/fetch tools (OAuth-gated) | auto | `cargo test -p kigi-tools --lib web_search` and `--lib web_fetch` (Kimi wire contracts) |
| 18 | Performance budgets | auto | `scripts/bench.sh` — `kigi --version` p95 ≤ 50 ms, TUI first frame ≤ 300 ms (CI `perf` job) |
| 19 | Coexistence with official kimi-cli | manual | both logged in on one machine; kigi never reads/writes `~/.kimi` or keyring service `kimi-code` (F7 import is read-only; §9 requires a 24 h parallel-use pass) |
