#!/usr/bin/env bash
# samply profile each bench in JIT-on and JIT-off, save firefox profile + parse top self-time.
set -u
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$ROOT/target/release/otter"
SCRIPTS="$ROOT/benchmarks/scripts"
OUT="$ROOT/benchmarks/profiles"
RATE=4000
do_one() {
  local script="$1" mode="$2" jitenv="$3"
  local base="${script%.*}.$mode"
  echo "=== $base ==="
  env $jitenv samply record --save-only -r $RATE -o "$OUT/$base.json.gz" -- "$BIN" run "$SCRIPTS/$script" >/dev/null 2>&1
  node "$OUT/parse.mjs" "$OUT/$base.json.gz" 30 2>&1 | sed "s/^/  /"
}
for s in "$@"; do
  do_one "$s" on "OTTER_JIT=1"
  do_one "$s" off ""
done
