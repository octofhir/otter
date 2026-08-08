#!/usr/bin/env bash
# The absolute bar: otter against node and bun on the same kernel.
#
# Internal tier ratios say whether a change helped us. They say nothing about
# whether the engine is fast. This does. Every kernel is timed the same way in
# every engine: warm up, then take the median of N timed invocations of
# engineKernel, startup excluded on both sides.
#
# Usage:
#   scripts/vs.sh                    # all kernels
#   scripts/vs.sh dense-array        # one kernel
#   TIER=template scripts/vs.sh      # otter tier under test (default tiered)
set -euo pipefail

cd "$(dirname "$0")/.."

BIN=target/release/otter-engine-benchmark
TIER="${TIER:-production-tiered}"
SAMPLES="${SAMPLES:-20}"
WARMUP="${WARMUP:-8}"

KERNELS=(
  "method-call-monomorphic 500003500000"
  "numeric-leaf           -700000"
  "typed-parameter-loop    300000"
  "branch-phi             -6000000"
  "dense-array            5234688"
  "boxed-double-property  4000000"
  "property-polymorphic   80011800000"
  "native-boundary       27000000"
)

if [[ ! -x "$BIN" ]]; then
  echo "building $BIN" >&2
  cargo build --release -q -p otter-benchmark --features engine \
    --bin otter-engine-benchmark
fi

filter="${1:-}"

# Median wall-clock milliseconds per engineKernel() invocation, measured
# inside the host engine so process startup is excluded.
external_ms() {
  local engine="$1" source_path="$2" harness
  command -v "$engine" >/dev/null 2>&1 || { echo "-"; return; }
  harness=$(mktemp -t vs-kernel).mjs
  cat "$source_path" >"$harness"
  cat >>"$harness" <<HARNESS

for (let w = 0; w < $WARMUP; w++) engineKernel();
const samples = [];
for (let i = 0; i < $SAMPLES; i++) {
  const started = process.hrtime.bigint();
  engineKernel();
  samples.push(Number(process.hrtime.bigint() - started) / 1e6);
}
samples.sort((a, b) => a - b);
console.log(samples[samples.length >> 1].toFixed(4));
HARNESS
  "$engine" "$harness" 2>/dev/null | tail -1 || echo "-"
  rm -f "$harness"
}

otter_ms() {
  local source_path="$1" expected="$2"
  "$BIN" kernel \
    --source "$source_path" \
    --function engineKernel \
    --expected "$expected" \
    --jit-tier "$TIER" \
    --samples "$SAMPLES" \
    --warmup "$WARMUP" 2>/dev/null |
    python3 -c "
import json, statistics, sys
record = json.load(sys.stdin)
if record.get('failure'):
    print('-'); sys.exit(0)
wall = next(m for m in record['metrics'] if m['name'] == 'wall-time')
print(f\"{statistics.median(wall['samples']) / 1e6:.4f}\")
"
}

printf 'otter tier: %s   samples=%s warmup=%s   ms per engineKernel(), lower is better\n\n' \
  "$TIER" "$SAMPLES" "$WARMUP"
printf '%-24s %10s %10s %10s %12s %12s\n' \
  kernel otter node bun otter/node otter/bun

for entry in "${KERNELS[@]}"; do
  read -r name expected <<<"$entry"
  if [[ -n "$filter" && "$name" != *"$filter"* ]]; then continue; fi
  source_path="benchmarks/scripts/${name}.js"

  ms_otter=$(otter_ms "$source_path" "$expected")
  ms_node=$(external_ms node "$source_path")
  ms_bun=$(external_ms bun "$source_path")

  read -r vs_node vs_bun <<<"$(python3 -c "
def ratio(ours, theirs):
    try:
        ours, theirs = float(ours), float(theirs)
    except ValueError:
        return '-'
    return f'{ours / theirs:.2f}x' if theirs else '-'
print(ratio('$ms_otter', '$ms_node'), ratio('$ms_otter', '$ms_bun'))
")"

  printf '%-24s %10s %10s %10s %12s %12s\n' \
    "$name" "$ms_otter" "$ms_node" "$ms_bun" "$vs_node" "$vs_bun"
done

cat <<'NOTE'

Ratios above 1.00x mean otter is slower by that factor. This is the number
that decides whether the engine is getting good; internal tier ratios only
decide whether a given change helped.
NOTE
