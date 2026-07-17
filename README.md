# Kigi

**Kigi** (`kigi`) is an unofficial Kimi Code CLI community build — a
terminal-based AI coding agent based on the Apache-2.0 sources of
[xai-org/grok-build](https://github.com/xai-org/grok-build), re-targeted at
the Kimi Code subscription API and the Moonshot open platform.

It runs as a full-screen TUI that understands your codebase, edits files,
executes shell commands, and manages long-running tasks — interactively,
headlessly for scripting/CI, or embedded in editors via the Agent Client
Protocol (ACP).

Kigi is not affiliated with Moonshot AI or xAI. It coexists with the official
`kimi` CLI on the same machine: independent binary name, independent config
directory (`~/.kigi`), independent keyring credentials, and a `KIGI_*`
environment-variable namespace. Nothing the official client installs or
stores is touched.

## Status

`0.1.0` — milestone M0 (compilable skeleton) complete:

- 62 workspace crates renamed to the `kigi-*` namespace; brand and env-var
  namespaces separated from upstream.
- Telemetry, voice input, announcements, plugin marketplace, and
  relay/gateway remote services removed. Kigi is **zero-telemetry**: the
  only outbound connections are the inference/auth APIs you configure,
  GitHub Releases for updates, and MCP servers you add.
- Toolchain Rust 1.97.0; `cargo check`/`clippy --workspace --all-targets`
  clean; `cargo deny check advisories` gate in place.

Authentication and inference against Kimi Code (milestone M1), the
compatibility surface (M2), and release distribution (M3) are in progress.

## Providers and API keys

Kigi talks to a fixed three-platform registry:

| Platform id   | Base URL                        | Auth                        |
| ------------- | ------------------------------- | --------------------------- |
| `kimi-code`   | `https://api.kimi.com/coding/v1` | Kimi Code subscription OAuth (`kigi login`) |
| `moonshot-cn` | `https://api.moonshot.cn/v1`    | Moonshot open-platform API key |
| `moonshot-ai` | `https://api.moonshot.ai/v1`    | Moonshot open-platform API key |

Moonshot API keys come from the environment or `~/.kigi/config.toml`
(environment wins; values are never logged):

```sh
export KIGI_MOONSHOT_API_KEY=sk-...     # applies to both open platforms
export KIGI_MOONSHOT_CN_API_KEY=sk-...  # platform-scoped, beats the generic name
export KIGI_MOONSHOT_AI_API_KEY=sk-...
```

```toml
# ~/.kigi/config.toml
[platforms.moonshot-cn]
api_key = "sk-..."

[platforms.moonshot-ai]
api_key = "sk-..."
```

On login and on startup Kigi syncs each configured platform's model list
from `GET {base}/models` and shows the merged catalog in the model picker
(catalog keys are `{platform_id}/{model_id}`). If the sync fails, the last
cached catalog is used; with no cache, a small built-in fallback list
applies. Model selection resolves as
`--model` CLI flag > `KIGI_DEFAULT_MODEL` > `[models] default` in
config.toml > server-delivered list > built-in fallback.

`KIGI_CODE_BASE_URL` re-points the subscription platform (useful for
testing); `KIGI_MOONSHOT_CN_BASE_URL` / `KIGI_MOONSHOT_AI_BASE_URL` are the
equivalent dev/test overrides for the open platforms.

## Building from source

```sh
rustup toolchain install 1.97.0
cargo build --profile release-dist -p kigi-bin
./target/release-dist/kigi --version
```

`protoc` is invoked through the vendored [dotslash](https://dotslash-cli.com)
launcher at `bin/protoc`; install dotslash (`brew install dotslash` or
`cargo install dotslash`) if it is not already on your PATH.

## License

Apache-2.0. See [LICENSE](LICENSE), [NOTICE](NOTICE), and
[THIRD-PARTY-NOTICES](THIRD-PARTY-NOTICES). Code ported from
openai/codex and sst/opencode is documented in
[crates/codegen/kigi-tools/THIRD_PARTY_NOTICES.md](crates/codegen/kigi-tools/THIRD_PARTY_NOTICES.md).
