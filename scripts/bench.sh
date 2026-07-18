#!/usr/bin/env bash
# Performance budgets (PRD §7.3, enforced in CI from M2):
#   1. `kigi --version` p95 ≤ 50 ms (hyperfine)
#   2. TUI first frame ≤ 300 ms (PTY probe: spawn → first welcome paint)
#
# Usage: scripts/bench.sh [path-to-kigi-binary]
# Defaults to target/release/kigi. Exits non-zero on any budget miss.
set -euo pipefail

KIGI_BIN="${1:-target/release/kigi}"
VERSION_P95_BUDGET_MS=50
FIRST_FRAME_BUDGET_MS=300

if [[ ! -x "$KIGI_BIN" ]]; then
    echo "error: kigi binary not found at $KIGI_BIN (build with: cargo build --release -p kigi-bin)" >&2
    exit 2
fi

echo "── budget 1: '$KIGI_BIN --version' p95 ≤ ${VERSION_P95_BUDGET_MS}ms ──"
json=$(mktemp)
hyperfine --warmup 3 --runs 30 --export-json "$json" "$KIGI_BIN --version" >/dev/null
p95_ms=$(python3 - "$json" <<'PY'
import json, sys, statistics
times = json.load(open(sys.argv[1]))["results"][0]["times"]
qs = statistics.quantiles(times, n=20)  # qs[18] = p95
print(f"{qs[18] * 1000:.1f}")
PY
)
rm -f "$json"
echo "p95 = ${p95_ms}ms"
awk -v p="$p95_ms" -v b="$VERSION_P95_BUDGET_MS" 'BEGIN { exit !(p <= b) }' || {
    echo "FAIL: --version p95 ${p95_ms}ms exceeds ${VERSION_P95_BUDGET_MS}ms" >&2
    exit 1
}

echo "── budget 2: TUI first frame ≤ ${FIRST_FRAME_BUDGET_MS}ms ──"
# Measured through the pty harness (real vt100 emulator answering terminal
# queries): the leading wait_for_text step's elapsed_ms IS spawn → first
# painted welcome frame. See scripts/first-frame.scenario.json.
PTY_SCENARIO="${PTY_SCENARIO:-target/release/pty-scenario}"
if [[ ! -x "$PTY_SCENARIO" ]]; then
    echo "error: pty-scenario not found at $PTY_SCENARIO (build with: cargo build --release -p kigi-pager-pty-harness --bin pty-scenario)" >&2
    exit 2
fi
artifacts=$(mktemp -d)
report=$("$PTY_SCENARIO" \
    --scenario scripts/first-frame.scenario.json \
    --binary "$(cd "$(dirname "$KIGI_BIN")" && pwd)/$(basename "$KIGI_BIN")" \
    --artifacts "$artifacts")
rm -rf "$artifacts"
first_ms=$(python3 - "$report" <<'PY'
import json, sys
d = json.loads(sys.argv[1][sys.argv[1].index("{"):])
assert d["status"] == "passed", f"scenario failed: {d}"
print(d["steps"][0]["elapsed_ms"])
PY
)
echo "first frame = ${first_ms}ms"
awk -v p="$first_ms" -v b="$FIRST_FRAME_BUDGET_MS" 'BEGIN { exit !(p <= b) }' || {
    echo "FAIL: first frame ${first_ms}ms exceeds ${FIRST_FRAME_BUDGET_MS}ms" >&2
    exit 1
}

echo "PASS: all performance budgets met"
