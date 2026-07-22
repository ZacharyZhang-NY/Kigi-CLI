<div align="center">

<h1>Kigi (<code>kigi</code>) 🌘</h1>

<h3>🕸️ The world's first CLI with built-in <em>Graph Engineering</em></h3>

<p><code>/graph</code> turns one objective into a dependency graph of
autonomous, self-verifying agent loops — planned, parallelized,
adversarially verified, and merged back, end to end.</p>

**Kigi** is a coding agent that lives in your terminal. It reads the repo,
writes the patch, runs the tests, and keeps going while you do something else.
Full-screen, headless in CI with `-p`, or docked in your editor over ACP.

**Already paying for Claude Pro/Max, ChatGPT Plus/Pro, GitHub Copilot, or
Grok? Sign in and use it.** No API key, no second bill. Rather bring your own
key? OpenAI, Anthropic, Google, DeepSeek, Groq, Moonshot and
[two dozen more](#providers-and-api-keys) are wired in.

[Installation](#installation) ·
[Graph engineering](#graph-engineering) ·
[Providers and API keys](#providers-and-api-keys) ·
[Building from source](#building-from-source) ·
[Coexistence with the official CLI](#coexistence-with-the-official-kimi-cli) ·
[License](#license)

![Kigi demo](video/Kigi.gif)

[Full-quality recording (mp4)](video/Kigi.mp4)

</div>

---

## Installation

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/ZacharyZhang-NY/Kigi-CLI/main/install.sh | bash
```

```powershell
# Windows PowerShell
irm https://raw.githubusercontent.com/ZacharyZhang-NY/Kigi-CLI/main/install.ps1 | iex
```

```sh
kigi login   # pick a provider, sign in
kigi         # go
```

Single file, no runtime. macOS and Linux on arm64/x86_64, Windows on x86_64,
checksummed against the release's `SHA256SUMS`. `kigi update` handles upgrades.

## Graph engineering

Every other agent runs a loop: think, act, repeat — one thread, one thing at a
time. `/graph` runs a dependency graph instead. Work that doesn't block other
work happens at the same time, in separate worktrees, and nothing merges until
something else has tried to tear it apart.

```
/graph <objective> [--budget <tokens>]   # decompose + run fully autonomously
/graph status                            # node tree, budget, current work
/graph show                              # box-drawing DAG view
/graph pause | resume [--budget <n>]     # halt / continue (budget top-up)
/graph clear                             # abandon the graph
```

One command runs the whole thing, start to finish:

- A planner breaks your objective into a dependency DAG, then validates it.
- Independent nodes fan out as parallel workers, each in its own git worktree.
- Every node has to get past an adversarial verifier before it merges back.
- Find something out of scope? Say `DISCOVERED:` and the graph replans —
  append-only, so nothing already agreed on gets rewritten.
- Between passes, a topology optimizer drops dependencies that were never real.
- A final node re-checks the *whole* objective before the graph is allowed to
  call itself done.

State lives in `.kigi/graph.jsonl`, next to your code. Close the laptop, come
back tomorrow, `/graph resume`. A teammate can pick it up from the same file.

On by default. `KIGI_GRAPH=0` turns it off; `KIGI_GRAPH_CONCURRENCY` (default
3) controls how many nodes run at once.

## Providers and API keys

29 platforms ship compiled in: 5 you sign into, 24 you hand a key. Nothing is
registered at runtime — if it's not in this list, it's not there.

**Sign in with a subscription you already pay for.** Run `kigi login` and pick.
Each provider's token is stored under its own key, and one provider's
credentials are never sent to another.

| Platform id      | Provider                  | Sign-in                                     |
| ---------------- | ------------------------- | ------------------------------------------- |
| `kimi-code`      | Kimi Code (original target)| Subscription OAuth (device code)           |
| `claude-pro-max` | Claude Pro/Max            | Subscription OAuth (browser, PKCE)          |
| `openai-codex`   | ChatGPT Plus/Pro (Codex)  | Subscription OAuth (browser, PKCE)          |
| `github-copilot` | GitHub Copilot            | Subscription OAuth (device code)            |
| `xai-grok`       | xAI Grok                  | Subscription OAuth (device code)            |

You get whatever models your plan actually serves — the list is fetched at
sign-in, not hardcoded. (ChatGPT/Codex is the exception: its backend publishes
no model endpoint, so those four are compiled in.)

**API-key providers.** Export the env var, or drop the key in
`~/.kigi/config.toml`. Keys are never logged.

| Provider                  | Platform id            | API key env                                     |
| ------------------------- | ---------------------- | ----------------------------------------------- |
| Moonshot (moonshot.cn)    | `moonshot-cn`          | `KIGI_MOONSHOT_CN_API_KEY` (or `KIGI_MOONSHOT_API_KEY`) |
| Moonshot (moonshot.ai)    | `moonshot-ai`          | `KIGI_MOONSHOT_AI_API_KEY` (or `KIGI_MOONSHOT_API_KEY`) |
| OpenAI                    | `openai`               | `OPENAI_API_KEY`                                |
| Anthropic                 | `anthropic`            | `ANTHROPIC_API_KEY`                             |
| DeepSeek                  | `deepseek`             | `DEEPSEEK_API_KEY`                              |
| Groq                      | `groq`                 | `GROQ_API_KEY`                                  |
| Mistral                   | `mistral`              | `MISTRAL_API_KEY`                               |
| Fireworks AI              | `fireworks`            | `FIREWORKS_API_KEY`                             |
| Google Gemini             | `google`               | `GEMINI_API_KEY`                                |
| OpenRouter                | `openrouter`           | `OPENROUTER_API_KEY`                            |
| Together AI               | `together`             | `TOGETHER_API_KEY`                              |
| Cerebras                  | `cerebras`             | `CEREBRAS_API_KEY`                              |
| NVIDIA NIM                | `nvidia`               | `NVIDIA_API_KEY`                                |
| Vercel AI Gateway         | `vercel-ai-gateway`    | `AI_GATEWAY_API_KEY`                            |
| xAI (Grok)                | `xai`                  | `XAI_API_KEY`                                   |
| Qwen Token Plan           | `qwen-token-plan`      | `QWEN_TOKEN_PLAN_API_KEY`                       |
| Qwen Token Plan (China)   | `qwen-token-plan-cn`   | `QWEN_TOKEN_PLAN_CN_API_KEY`                    |
| Kimi For Coding           | `kimi-coding`          | `KIMI_API_KEY`                                  |
| Z.AI                      | `zai`                  | `ZAI_API_KEY`                                   |
| Z.AI Coding (China)       | `zai-coding-cn`        | `ZAI_CODING_CN_API_KEY`                         |
| Xiaomi MiMo               | `xiaomi`               | `XIAOMI_API_KEY`                                |
| Xiaomi Token Plan (China) | `xiaomi-token-plan-cn` | `XIAOMI_TOKEN_PLAN_CN_API_KEY`                  |
| MiniMax                   | `minimax`              | `MINIMAX_API_KEY`                               |
| MiniMax (China)           | `minimax-cn`           | `MINIMAX_CN_API_KEY`                            |

```sh
export OPENAI_API_KEY=sk-...
export XAI_API_KEY=xai-...
```

```toml
# ~/.kigi/config.toml
[platforms.openai]
api_key = "sk-..."

[platforms.xai]
api_key = "xai-..."
```

Model lists sync on startup. Pick one with `/model`, set its thinking level
with `/effort`.

Web `search`/`fetch` need a Kimi Code subscription; API-key sessions run
without them, same as the official client.

## Building from source

```sh
rustup toolchain install 1.97.0
cargo build --profile release-dist -p kigi-bin
./target/release-dist/kigi --version
```

`protoc` is invoked through the vendored [dotslash](https://dotslash-cli.com)
launcher at `bin/protoc`; install dotslash (`brew install dotslash` or
`cargo install dotslash`) if it is not already on your PATH.

## Coexistence with the official Kimi CLI

Kigi started as an unofficial Kimi Code CLI — a community fork of
[xai-org/grok-build](https://github.com/xai-org/grok-build), not affiliated
with Moonshot AI or xAI. It keeps its own binary, its own `~/.kigi`, its own
keyring entry, and its own `KIGI_*` env vars, and never touches what the
official `kimi` CLI installed. `kigi import-kimi` copies your old config over
once, read-only.

**Zero telemetry.** It talks to the APIs you configured, GitHub Releases, and
your own MCP servers. Nothing else.

## License

Apache-2.0. See [LICENSE](LICENSE), [NOTICE](NOTICE), and
[THIRD-PARTY-NOTICES](THIRD-PARTY-NOTICES.md). Code ported from
openai/codex and sst/opencode is documented in
[crates/codegen/kigi-tools/THIRD_PARTY_NOTICES.md](crates/codegen/kigi-tools/THIRD_PARTY_NOTICES.md).
Kigi is based on Grok Build Open Source; the `--version` output carries the
attribution.
