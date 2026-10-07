<div align="center">

<h1>Kigi (<code>kigi</code>) 🌘</h1>

<h3>🕸️ The world's first CLI with built-in <em>Graph Engineering</em></h3>

<p><code>/graph</code> turns one objective into a dependency graph of
autonomous, self-verifying agent loops — planned, parallelized,
adversarially verified, and merged back, end to end.</p>

**Kigi** is a coding agent that lives in your terminal. It reads the repo,
writes the patch, runs the tests, and keeps going while you do something else.
Full-screen, headless in CI with `-p`, or docked in your editor over ACP.

**Already paying for Claude Pro/Max, ChatGPT Plus/Pro, GitHub Copilot,
Grok, or Devin? Sign in and use it.** No API key, no second bill. Rather bring your own
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
curl -fsSL https://kigicli.dev/install.sh | bash
```

```powershell
# Windows PowerShell
irm https://kigicli.dev/install.ps1 | iex
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

30 platforms ship compiled in: 6 you sign into, 24 you hand a key. Anything
else is an endpoint you declare yourself ([your own endpoint](#your-own-endpoint)).

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
| `devin`          | Devin                     | Subscription OAuth (browser, PKCE)          |

You get whatever models your plan actually serves — the list is fetched at
sign-in, not hardcoded. (ChatGPT/Codex is the exception: its backend publishes
no model endpoint, so those four are compiled in.)

**Devin.** Sign in through the built-in Kigi agent's `/login` provider
picker → **Devin (subscription)**, which opens the browser once; the
session token lives under `oauth/devin` and drives Devin's own model
catalog. (The standalone `kigi login` command remains Kimi Code sign-in.)
Devin serves *model families* — a named family (e.g. `SWE-2`) with several
selectable variants — so `/model` shows one row per family and Enter opens
its variant list, where each row inserts the exact `devin/<uid>` catalog
id. A bare family name (`/model SWE-2`, `/model devin/swe-2`) selects the
provider-flagged default variant; every concrete id —
`/model devin/<catalog-uid>` — and each variant's full display name still
work directly. Kigi runs the agent — tools, sessions, graph, and swarm
stay local; Devin serves inference only.

**Fusion.** Devin's catalog also advertises `fusion-…` router rows
(`Fusion (<lead> + <sidekick>)`). `/fusion` lists one row per lead; Enter
opens that lead's sidekick list and each row inserts the concrete
`devin/fusion-…` pair id (also accepted directly via `/model
devin/fusion-…`). Selecting a pair resolves the lead once per request via
Devin's `AssignModel`, then chats on the returned uid with the returned
assignment JWT — the router uid itself is never sent as a chat model.
Delegation is Kigi-local: an unpinned subagent spawned under a fusion
parent inherits the pair's configured sidekick model (explicit per-agent
pins, agent `model:` overrides, runtime/persona pins, and `fork_context`
all still win). This is Kigi's own implementation — it does not reproduce
the Devin CLI's helper prompts or loop, and helper-side inference under a
fusion parent is not verified to match Devin CLI Fusion behavior or
pricing — treat it as ordinary inference on the configured helper model.

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

### Your own endpoint

Anything that speaks the OpenAI Chat Completions API or the Anthropic Messages
API works: a gateway, a proxy, Ollama, LM Studio, vLLM. In `kigi`, open `/login`
(or the welcome login screen) and pick **Custom provider (OpenAI compatible)**
or **(Anthropic compatible)**, then enter a base URL, a name, and an API key.
Kigi saves the provider,
checks the key against `GET {base}/models`, and `/model` lists what the endpoint
serves as `name/model`.

```toml
# ~/.kigi/config.toml
[platforms.my-gateway]
base_url = "https://gateway.example.com/v1"   # include /v1 when the API lives there
api = "openai"                                # or "anthropic"
api_key = "sk-..."                            # or store it in ~/.kigi/auth.json under "my-gateway"
```

A key is required. A local server that ignores auth takes any value. The
context window is 256K unless the endpoint reports one. To remove a provider,
delete its table and its `auth.json` entry.

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

**Zero telemetry.** It talks to the APIs you configured, the Kigi release
server, and your own MCP servers. Nothing else.

## License

Apache-2.0. See [LICENSE](LICENSE), [NOTICE](NOTICE), and
[THIRD-PARTY-NOTICES](THIRD-PARTY-NOTICES.md). Code ported from
openai/codex and sst/opencode is documented in
[crates/codegen/kigi-tools/THIRD_PARTY_NOTICES.md](crates/codegen/kigi-tools/THIRD_PARTY_NOTICES.md).
Kigi is based on Grok Build Open Source; the `--version` output carries the
attribution.
