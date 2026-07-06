#!/usr/bin/env bash
#
# Run the classic V8 benchmark suite (v8-v7, from mozilla/arewefastyet) on the otter engine.
# Downloads the benchmark JS into ./v8-v7 (gitignored) on first run, builds the `otter` CLI in
# release mode, concatenates the suite into one realm, and prints per-benchmark scores plus the
# composite score. Higher is better; scores are normalized to a 2008 reference machine at 100.
#
#   scripts/run-v8bench.sh              # full suite
#   scripts/run-v8bench.sh richards     # one benchmark (any of the .js basenames)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/v8-v7"
RAW="https://raw.githubusercontent.com/mozilla/arewefastyet/master/benchmarks/v8-v7"
FILES=(base.js richards.js deltablue.js crypto.js raytrace.js earley-boyer.js regexp.js splay.js navier-stokes.js run.js)

if [ ! -f "$DEST/base.js" ]; then
  echo "Downloading v8-v7 benchmark into $DEST ..."
  mkdir -p "$DEST"
  for f in "${FILES[@]}"; do
    curl -fsSL "$RAW/$f" -o "$DEST/$f"
  done
fi

cargo build --release -q -p otter-cli --bin otter

if [ $# -ge 1 ]; then
  SUITES=("$@")
else
  SUITES=(richards deltablue crypto raytrace earley-boyer regexp splay navier-stokes)
fi

# The otter CLI runs a single entry file in one realm. Concatenate base.js (defines
# BenchmarkSuite), the selected benchmarks (each registers itself), a `print` shim (the upstream
# driver writes results with the shell `print()`), and run.js with its `load()` lines stripped.
COMBINED="$DEST/combined.js"
{
  cat "$DEST/base.js"
  for s in "${SUITES[@]}"; do
    cat "$DEST/${s%.js}.js"
  done
  printf 'var print = (...a) => console.log(a.join(" "));\n'
  sed '/^load(/d' "$DEST/run.js"
} > "$COMBINED"

exec "$ROOT/target/release/otter" run --timeout 0 "$COMBINED"
