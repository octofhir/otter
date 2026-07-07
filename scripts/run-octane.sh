#!/usr/bin/env bash
#
# Run the Octane 2.0 benchmark suite (chromium/octane) on the otter engine.
# Downloads the benchmark JS into ./octane (gitignored) on first run, builds the
# `otter` CLI in release mode, concatenates the suite into one realm, and prints
# per-benchmark scores plus the composite score. Higher is better; scores are
# normalized to a 2008 reference machine at 100, the composite is the geometric
# mean. Octane is a superset of the classic v8-v7 suite (adds Box2D, PdfJS,
# Mandreel, GB-EMU, CodeLoad, Typescript, zlib, RegExp variants).
#
#   scripts/run-octane.sh                 # full suite
#   scripts/run-octane.sh richards box2d  # named subset (suite names below)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/octane"
RAW="https://raw.githubusercontent.com/chromium/octane/master"

# Every JS file the upstream run.js loads, in its load order.
FILES=(
  base.js richards.js deltablue.js crypto.js raytrace.js earley-boyer.js
  regexp.js splay.js navier-stokes.js pdfjs.js mandreel.js
  gbemu-part1.js gbemu-part2.js code-load.js box2d.js
  zlib.js zlib-data.js typescript.js typescript-input.js typescript-compiler.js
)

# suite name -> file list (multi-file suites expand in load order).
suite_files() {
  case "$1" in
    gbemu)      echo "gbemu-part1.js gbemu-part2.js" ;;
    zlib)       echo "zlib.js zlib-data.js" ;;
    typescript) echo "typescript.js typescript-input.js typescript-compiler.js" ;;
    code-load)  echo "code-load.js" ;;
    *)          echo "$1.js" ;;
  esac
}

if [ ! -f "$DEST/base.js" ]; then
  echo "Downloading Octane benchmark into $DEST ..."
  mkdir -p "$DEST"
  for f in "${FILES[@]}"; do
    echo "  $f"
    curl -fsSL "$RAW/$f" -o "$DEST/$f"
  done
fi

cargo build --release -q -p otter-cli --bin otter

if [ $# -ge 1 ]; then
  SUITES=("$@")
else
  SUITES=(richards deltablue crypto raytrace earley-boyer regexp splay
          navier-stokes pdfjs mandreel gbemu code-load box2d zlib typescript)
fi

# The otter CLI runs a single entry file in one realm. Concatenate base.js
# (defines BenchmarkSuite), the selected benchmarks (each registers itself), a
# `print` shim (the upstream driver reports through the shell `print()`), and
# run.js with its `load()` lines stripped.
COMBINED="$DEST/combined.js"
{
  cat "$DEST/base.js"
  for s in "${SUITES[@]}"; do
    for f in $(suite_files "$s"); do
      cat "$DEST/$f"
    done
  done
  printf 'var print = (...a) => console.log(a.join(" "));\n'
  if [ ! -f "$DEST/run.js" ]; then
    curl -fsSL "$RAW/run.js" -o "$DEST/run.js"
  fi
  sed '/^load(/d' "$DEST/run.js"
} > "$COMBINED"

exec "$ROOT/target/release/otter" run --timeout 0 "$COMBINED"
