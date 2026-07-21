#!/usr/bin/env python3
"""Regenerate the bundled models.dev enrichment snapshot.

Usage:
  curl -s https://models.dev/api.json -o /tmp/modelsdev-api.json
  python3 scripts/gen_enrichment_snapshot.py /tmp/modelsdev-api.json

PURE FILTER, NO TRANSFORM: writes the raw models.dev per-provider objects
(only the providers kigi references) to
crates/codegen/kigi-models/enrichment_snapshot.json. All field
interpretation lives in ONE place — `kigi_models::enrichment::parse_api_json`
— which parses this snapshot and runtime refreshes identically.

Keep TARGETS in sync with the registry's `models_dev_id` values (a registry
test cross-checks coverage). `openrouter` is deliberately absent: its
/models wire serves context_length itself.
"""

import json
import pathlib
import sys

TARGETS = [
    "anthropic", "azure", "openai", "deepseek", "nvidia", "google",
    "amazon-bedrock", "mistral", "groq", "cerebras", "cloudflare-ai-gateway",
    "xai", "togetherai", "fireworks-ai", "opencode", "opencode-go",
    "kimi-for-coding", "moonshotai", "moonshotai-cn", "minimax", "minimax-cn",
    "alibaba-token-plan", "alibaba-token-plan-cn", "xiaomi",
    "xiaomi-token-plan-cn", "zai-coding-plan", "zhipuai-coding-plan",
    "github-copilot", "vercel",
]

# Per-model keys the Rust parser consumes; everything else is dropped to keep
# the bundled file lean (cost/date/experimental fields are dead weight).
MODEL_KEYS = ("name", "reasoning", "reasoning_options", "limit", "modalities",
              "tool_call")


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    src = json.load(open(sys.argv[1]))
    missing = [pid for pid in TARGETS if pid not in src]
    if missing:
        # Fail fast: a target vanishing from models.dev needs a human look.
        print(f"providers missing from api.json: {missing}", file=sys.stderr)
        return 1
    out = {}
    # Provenance stamp (parse_api_json skips keys starting with "_").
    out["_meta"] = {
        "source": "https://models.dev/api.json (github.com/sst/models.dev, MIT)",
        "note": "filtered snapshot for kigi model-metadata enrichment; "
                "regenerate via scripts/gen_enrichment_snapshot.py",
    }
    out |= {
        pid: {
            "models": {
                mid: {k: m[k] for k in MODEL_KEYS if k in m}
                for mid, m in src[pid]["models"].items()
            }
        }
        for pid in TARGETS
    }
    dst = pathlib.Path(__file__).resolve().parent.parent / (
        "crates/codegen/kigi-models/enrichment_snapshot.json"
    )
    dst.write_text(json.dumps(out, separators=(",", ":"), sort_keys=True) + "\n")
    providers = [k for k in out if not k.startswith("_")]
    n_models = sum(len(out[p]["models"]) for p in providers)
    print(f"wrote {dst}: {len(providers)} providers, {n_models} models, "
          f"{dst.stat().st_size} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
