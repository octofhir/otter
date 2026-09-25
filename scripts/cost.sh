#!/usr/bin/env bash
# Cost attribution for the engine kernels.
#
# Answers one question: for each kernel, how many retired instructions did we
# spend, and which axis explains them — bytecode dispatch, inline-cache
# misses, the native boundary, deopts, or GC.
#
# Retired instructions (not wall time) is the metric: it is stable across
# thermal state and background load on this host. Wall time is reported too,
# but only as a sanity check.
#
# Usage:
#   scripts/cost.sh                          # all kernels, all tiers
#   scripts/cost.sh dense-array              # one kernel, all tiers
#   TIERS=template scripts/cost.sh           # one tier
#   SAMPLES=40 scripts/cost.sh               # tighter medians
set -euo pipefail

cd "$(dirname "$0")/.."

TIERS="${TIERS:-interpreter template production-tiered}"
SAMPLES="${SAMPLES:-20}"
WARMUP="${WARMUP:-8}"

KERNELS=(
  "method-call-monomorphic 500003500000"
  "numeric-leaf           -700000"
  "math-intrinsics         2200000"
  "math-explicit-calls     2200000"
  "typed-parameter-loop    300000"
  "branch-phi             -6000000"
  "string-concat             600000"
  "dense-array            5234688"
  "boxed-double-property  4000000"
  "derived-constructor 10000200000"
  "property-polymorphic   80011800000"
  "native-boundary       27000000"
  "arith-exit-repair       150000"
  "polluted-feedback      4287494"
  "mixed-relational       4787500"
)

# Use Cargo's fresh artifact path, including configured target directories.
BIN=$(cargo build --release -q -p otter-benchmark --features engine \
  --bin otter-engine-benchmark --message-format=json-render-diagnostics |
  python3 -c '
import json, sys
executable = None
for line in sys.stdin:
    message = json.loads(line)
    if (message.get("reason") == "compiler-artifact"
            and message["target"]["name"] == "otter-engine-benchmark"):
        executable = message.get("executable")
if executable is None:
    sys.exit("Cargo did not produce the engine benchmark executable")
print(executable)
')

filter="${1:-}"

parse_record() {
  python3 - "$1" "$2" <<'PY'
import json, statistics, sys

record = json.load(open(sys.argv[1]))
# Counter diagnostics are captured once around the whole run, so they cover
# warmup plus sampled invocations. Normalize to one invocation or the numbers
# stop being comparable the moment SAMPLES changes.
invocations = max(1, int(sys.argv[2]))
outcome = record["outcome"]
if outcome["status"] != "validated" or outcome.get("failure"):
    print("FAIL " + json.dumps(outcome), file=sys.stderr)
    sys.exit(1)

by_name = {}
for metric in record["metrics"]:
    samples = metric.get("samples") or []
    by_name[metric["name"]] = statistics.median(samples) if samples else 0.0

def total(*names):
    return sum(by_name.get(name, 0.0) for name in names) / invocations

# Axes. Each is a count of events the engine chose to perform, not a time
# share: the point is to see which axis dominates and then attack it, not to
# pretend the counters add up to the instruction total.
axes = {
    "wall_ms": by_name.get("wall-time", 0.0) / 1e6,
    "work_units": total("vm-work-units"),
    "ic_miss": total(
        "property-ic-load-misses",
        "property-ic-store-misses",
    ),
    "ic_install": total(
        "property-ic-load-installs",
        "property-ic-store-installs",
    ),
    "ic_disable": total(
        "property-ic-load-disables",
        "property-ic-store-disables",
    ),
    # Classified generated-to-runtime crossings: leaf, allocation, reentrant.
    # Call/construct counters overlap this aggregate and must not be added.
    "jit_stub": total("jit-runtime-stub-transitions"),
    "alloc_ok": total("jit-alloc-value-stub-ok"),
    "alloc_miss": total("jit-alloc-value-stub-miss"),
    # Broken out because it is the metric Slice 1 must drive to zero: a
    # property access that leaves generated code to ask the runtime.
    "prop_stub": total("jit-runtime-property-stubs"),
    "deopt": total(
        "jit-optimized-deopts",
        "jit-generated-call-deopts",
        "jit-generated-template-deopts",
        "jit-generated-optimizing-deopts",
    ),
    "gc": total("full-gc-cycles"),
}
print(" ".join(f"{value:.0f}" if key != "wall_ms" else f"{value:.4f}"
                for key, value in axes.items()))
PY
}

failed=0
for tier in $TIERS; do
  printf '\n=== tier: %s  samples=%s warmup=%s ===\n' "$tier" "$SAMPLES" "$WARMUP"
  printf '%-24s %14s %9s %11s %9s %7s %7s %7s %9s %10s %10s %11s %6s\n' \
    kernel retired wall_ms work_units instr/work ic_miss ic_inst ic_disa jit_stub alloc_ok alloc_miss prop_stub deopt
  for entry in "${KERNELS[@]}"; do
    read -r name expected <<<"$entry"
    if [[ -n "$filter" && "$name" != *"$filter"* ]]; then continue; fi

    source_path="benchmarks/scripts/${name}.js"
    record=$(mktemp)
    timing=$(mktemp)

    # One run yields both channels: the JSON record on stdout, the kernel
    # counters inside it, and retired instructions on stderr from time -l.
    if ! /usr/bin/time -l "$BIN" kernel \
        --source "$source_path" \
        --function engineKernel \
        --expected "$expected" \
        --jit-tier "$tier" \
        --samples "$SAMPLES" \
        --warmup "$WARMUP" >"$record" 2>"$timing"; then
      printf '%-24s %14s\n' "$name" "RUN-FAILED"
      cat "$timing" >&2
      rm -f "$record" "$timing"
      failed=1
      continue
    fi

    retired=$(awk '/instructions retired/{print $1}' "$timing")
    if ! axes=$(parse_record "$record" "$((SAMPLES + WARMUP))"); then
      printf '%-24s %14s\n' "$name" "PARSE-FAILED"
      rm -f "$record" "$timing"
      failed=1
      continue
    fi
    read -r wall work_units ic_miss ic_install ic_disable jit_stub alloc_ok alloc_miss prop_stub deopt gc <<<"$axes"

    per_work_unit=$(python3 -c \
      "r=${retired:-0}; d=(${work_units:-0}) * ($SAMPLES + $WARMUP); print(f'{r/d:.1f}' if d else '-')")

    printf '%-24s %14s %9s %11s %9s %7s %7s %7s %9s %10s %10s %11s %6s\n' \
      "$name" "${retired:-?}" "$wall" "$work_units" "$per_work_unit" \
      "$ic_miss" "$ic_install" "$ic_disable" "$jit_stub" "$alloc_ok" "$alloc_miss" \
      "$prop_stub" "$deopt"

    if [[ "${gc:-0}" != "0" ]]; then
      printf '%-24s %14s gc-cycles=%s\n' "" "" "$gc"
    fi

    rm -f "$record" "$timing"
  done
done

cat <<'NOTE'

Axes are event counts, not time shares. Read them as "where the engine is
doing work it should not have to", then take the top line into the next
declaration.
NOTE
exit "$failed"
