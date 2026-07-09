#!/usr/bin/env bash
#
# Run the classic V8 v7 benchmark suite from Mozilla AreWeFastYet.
# Higher scores are better. The Otter invocation uses native multi-file CLI
# loading: `otter run base.js suite.js ... driver.js`.

set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

DEST="$CACHE_DIR/v8-v7"
RAW="https://raw.githubusercontent.com/mozilla/arewefastyet/master/benchmarks/v8-v7"
FILES=(base.js richards.js deltablue.js crypto.js raytrace.js earley-boyer.js regexp.js splay.js navier-stokes.js run.js)

if [ ! -f "$DEST/base.js" ]; then
  echo "fetching V8 v7 into $DEST" >&2
  mkdir -p "$DEST"
  for f in "${FILES[@]}"; do
    curl -fsSL "$RAW/$f" -o "$DEST/$f"
  done
fi

if [ "$#" -gt 0 ]; then
  SUITES=("$@")
else
  SUITES=(richards deltablue crypto raytrace earley-boyer regexp splay navier-stokes)
fi

SUITE_FILES=()
for suite in "${SUITES[@]}"; do
  file="$DEST/${suite%.js}.js"
  if [ ! -f "$file" ]; then
    echo "error: unknown V8 v7 suite: $suite ($file not found)" >&2
    exit 2
  fi
  SUITE_FILES+=("$file")
done

DRIVER="$DEST/driver.js"
perl -pe 's/^load\(.*\n//; s/(?<![A-Za-z0-9_\$])print\(/console.log(/g' "$DEST/run.js" > "$DRIVER"

OTTER="$(ensure_otter_bin)"
OUT="$RESULTS_DIR/v8-v7-$(timestamp).log"
COMBINED="$DEST/combined.js"
{
  cat "$DEST/base.js"
  for file in "${SUITE_FILES[@]}"; do
    cat "$file"
  done
  cat "$DRIVER"
} > "$COMBINED"

status=0
set +e
{
  echo "=== node ==="
  run_external_file node "$COMBINED"
  echo "=== bun ==="
  run_external_file bun "$COMBINED"
  echo "=== otter ==="
  run_otter_files "$OTTER" "$DEST/base.js" "${SUITE_FILES[@]}" "$DRIVER"
} 2>&1 | tee "$OUT"
pipe_status=("${PIPESTATUS[@]}")
set -e
if [ "${pipe_status[0]}" -ne 0 ]; then
  status="${pipe_status[0]}"
fi

if ! awk '/^=== otter ===/{in_otter=1; next} /^=== /{in_otter=0} in_otter && /^Score \(version [0-9]+\): /{found=1} END{exit found ? 0 : 1}' "$OUT"; then
  echo "error: Otter V8 v7 completed without reporting a composite score" >&2
  status=1
fi

echo "wrote $OUT" >&2
exit "$status"
