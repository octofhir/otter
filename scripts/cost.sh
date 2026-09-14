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

BIN=target/release/otter-engine-benchmark
TIERS="${TIERS:-interpreter template production-tiered}"
SAMPLES="${SAMPLES:-20}"
WARMUP="${WARMUP:-8}"

KERNELS=(
  "method-call-monomorphic 500003500000"
  "numeric-leaf           -700000"
  "typed-parameter-loop    300000"
  "branch-phi             -6000000"
  "string-concat             600000"
  "dense-array            5234688"
  "boxed-double-property  4000000"
  "derived-constructor 10000200000"
  "property-polymorphic   80011800000"
  "native-boundary       27000000"
)

if [[ ! -x "$BIN" ]]; then
  echo "building $BIN" >&2
  cargo build --release -q -p otter-benchmark --features engine \
    --bin otter-engine-benchmark
fi

filter="${1:-}"

parse_record() {
  python3 - "$1" "$2" <<'PY'
import json, statistics, sys

record = json.load(open(sys.argv[1]))
# Counter diagnostics are captured once around the whole run, so they cover
# warmup plus sampled invocations. Normalize to one invocation or the numbers
# stop being comparable the moment SAMPLES changes.
invocations = max(1, int(sys.argv[2]))
if record.get("failure"):
    print("FAIL " + json.dumps(record["failure"]), file=sys.stderr)
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
    "reductions": total("vm-reductions"),
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
    # jit-runtime-stub-transitions is the aggregate of the leaf/alloc/
    # reentrant families, so summing it with its members would count the
    # same transition several times.
    "native": total(
        "jit-runtime-stub-transitions",
        "jit-to-rust-call-transitions",
        "jit-runtime-calls",
        "jit-runtime-constructs",
    ),
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

for tier in $TIERS; do
  printf '\n=== tier: %s  samples=%s warmup=%s ===\n' "$tier" "$SAMPLES" "$WARMUP"
  printf '%-24s %14s %9s %11s %9s %7s %7s %7s %9s %10s %10s %11s %6s\n' \
    kernel retired wall_ms reductions instr/red ic_miss ic_inst ic_disa native alloc_ok alloc_miss prop_stub deopt
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
      continue
    fi

    retired=$(awk '/instructions retired/{print $1}' "$timing")
    if ! axes=$(parse_record "$record" "$((SAMPLES + WARMUP))"); then
      printf '%-24s %14s\n' "$name" "PARSE-FAILED"
      rm -f "$record" "$timing"
      continue
    fi
    read -r wall reductions ic_miss ic_install ic_disable native alloc_ok alloc_miss prop_stub deopt gc <<<"$axes"

    per_reduction=$(python3 -c \
      "r=${retired:-0}; d=${reductions:-0}; print(f'{r/d:.1f}' if d else '-')")

    printf '%-24s %14s %9s %11s %9s %7s %7s %7s %9s %10s %10s %11s %6s\n' \
      "$name" "${retired:-?}" "$wall" "$reductions" "$per_reduction" \
      "$ic_miss" "$ic_install" "$ic_disable" "$native" "$alloc_ok" "$alloc_miss" \
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
