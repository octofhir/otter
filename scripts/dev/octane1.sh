#!/usr/bin/env bash
#
# Run one Octane suite under Otter and print its score.
#
#   scripts/dev/octane1.sh <suite> [tiered|jitless|interpreter] [extra otter flags...]
#
# Environment:
#   OTTER_BIN   binary to run (default target/release/otter)
#   CAP         hard wall-clock cap in seconds (default 300)
#   RUNS        number of runs; the median score is printed last (default 1)
#
# The suite is assembled the way benchmarks/run-octane.sh does it: base.js,
# the suite files with `print(` rewritten to `console.log(`, then the run.js
# driver without its `load(` lines. The assembled files live under
# benchmarks/results/octane1/<suite>/ (ignored).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OCTANE="${OCTANE:-$ROOT/benchmarks/.suite-cache/octane}"
BIN="${OTTER_BIN:-$ROOT/target/release/otter}"
CAP="${CAP:-300}"
RUNS="${RUNS:-1}"

suite="${1:?usage: octane1.sh <suite> [tiered|jitless|interpreter] [flags...]}"
shift
mode="${1:-tiered}"
[ "$#" -gt 0 ] && shift

case "$suite" in
  gbemu) files=(gbemu-part1.js gbemu-part2.js) ;;
  zlib) files=(zlib.js zlib-data.js) ;;
  typescript) files=(typescript.js typescript-input.js typescript-compiler.js) ;;
  *) files=("$suite.js") ;;
esac

work="$ROOT/benchmarks/results/octane1/$suite"
mkdir -p "$work"
cp "$OCTANE/base.js" "$work/base.js"
paths=("$work/base.js")
for f in "${files[@]}"; do
  perl -pe 's/(?<![A-Za-z0-9_\$])print\(/console.log(/g' "$OCTANE/$f" > "$work/$f"
  paths+=("$work/$f")
done
perl -pe 's/^load\(.*\n//; s/(?<![A-Za-z0-9_\$])print\(/console.log(/g' \
  "$OCTANE/run.js" > "$work/driver.js"
paths+=("$work/driver.js")

flags=()
case "$mode" in
  tiered) ;;
  jitless) flags+=(--jitless) ;;
  interpreter) flags+=(--interpreter) ;;
  *) echo "unknown mode: $mode" >&2; exit 2 ;;
esac

scores=()
for ((i = 0; i < RUNS; i++)); do
  set +e
  out="$(perl -e 'alarm shift; exec @ARGV' "$CAP" "$BIN" ${flags[@]+"${flags[@]}"} "$@" run "${paths[@]}" 2>&1)"
  status=$?
  set -e
  score="$(printf '%s\n' "$out" | sed -n 's/^Score (version [0-9]*): *//p' | tail -1)"
  if [ -z "$score" ]; then
    printf '%s\n' "$out" | tail -20 >&2
    echo "$suite $mode: no score (exit $status)"
    exit 1
  fi
  echo "$suite $mode run$i: $score"
  scores+=("$score")
done
median="$(printf '%s\n' "${scores[@]}" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}')"
echo "$suite $mode median: $median"
