#!/usr/bin/env bash
#
# Run Google's Octane benchmark suite. Higher scores are better.
# Suites run unmodified after a d8-shell prelude (`print`, `read`).
# Set OCTANE=/path/to/octane to use an existing checkout.

set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

OCTANE="${OCTANE:-$CACHE_DIR/octane}"

if [ ! -d "$OCTANE" ]; then
  echo "cloning Octane into $OCTANE" >&2
  git clone https://github.com/chromium/octane "$OCTANE"
fi
if [ ! -f "$OCTANE/base.js" ] || [ ! -f "$OCTANE/run.js" ]; then
  echo "error: Octane checkout at $OCTANE is missing base.js or run.js" >&2
  exit 1
fi

if [ "$#" -gt 0 ]; then
  SUITES=("$@")
else
  SUITES=(richards deltablue crypto raytrace earley-boyer regexp splay navier-stokes \
          pdfjs mandreel gbemu code-load box2d zlib typescript)
fi

suite_files() {
  local suite="$1"
  case "${suite%.js}" in
    gbemu) files=(gbemu-part1.js gbemu-part2.js) ;;
    zlib) files=(zlib.js zlib-data.js) ;;
    typescript) files=(typescript.js typescript-input.js typescript-compiler.js) ;;
    *) files=("${suite%.js}.js") ;;
  esac
  for f in "${files[@]}"; do
    if [ ! -f "$OCTANE/$f" ]; then
      echo "error: unknown/missing Octane suite file: $OCTANE/$f" >&2
      exit 2
    fi
    printf '%s\n' "$OCTANE/$f"
  done
}

for suite in "${SUITES[@]}"; do
  suite_files "$suite" >/dev/null
done

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
DRIVER="$WORK/octane-driver.js"
perl -pe 's/^load\(.*\n//' "$OCTANE/run.js" > "$DRIVER"
# Octane targets the d8 shell. Suites run unmodified after a prelude that
# provides the shell's `print` / `read`: emscripten suites (zlib) bind them
# inside eval'd code, and rewriting `print(` in sources corrupts the
# TypeScript suite's compiler input.
PRELUDE="$WORK/octane-prelude.js"
cat > "$PRELUDE" <<'JS'
if (typeof print === "undefined") globalThis.print = function (s) { console.log(s); };
if (typeof read === "undefined") globalThis.read = function (name) { throw new Error("read(" + name + ") is not available"); };
JS

OTTER="$(ensure_otter_bin)"
OUT="$RESULTS_DIR/octane-$(timestamp).log"

run_capped() {
  local label="$1"
  shift
  local tmp="$WORK/${label//[^A-Za-z0-9_.-]/_}.out"
  echo "=== $label ==="
  set +e
  "$@" > "$tmp" 2>&1
  local status=$?
  set -e
  if [ "$status" -eq 0 ]; then
    cat "$tmp"
  else
    awk 'NR <= 80 { print } END { if (NR > 80) print "... truncated failure output (" NR " lines)" }' "$tmp"
    echo "exit_code: $status"
  fi
}

run_suite() {
  local suite="$1"
  local suite_work="$WORK/$suite"
  mkdir -p "$suite_work"
  local suite_paths=()
  local file
  while IFS= read -r file; do
    suite_paths+=("$file")
  done < <(suite_files "$suite")

  local combined="$suite_work/octane-combined.js"
  {
    cat "$PRELUDE"
    cat "$OCTANE/base.js"
    for file in "${suite_paths[@]}"; do
      cat "$file"
    done
    cat "$DRIVER"
  } > "$combined"

  echo "### $suite"
  run_capped "node" run_external_file node "$combined"
  run_capped "bun" run_external_file bun "$combined"
  run_capped "otter" run_otter_files "$OTTER" "$PRELUDE" "$OCTANE/base.js" "${suite_paths[@]}" "$DRIVER"
}

status=0
set +e
{
  for suite in "${SUITES[@]}"; do
    run_suite "$suite"
  done
} 2>&1 | tee "$OUT"
pipe_status=("${PIPESTATUS[@]}")
set -e
if [ "${pipe_status[0]}" -ne 0 ]; then
  status="${pipe_status[0]}"
fi

if grep -Eq '^exit_code: [1-9][0-9]*$' "$OUT"; then
  echo "error: Octane reported benchmark failure" >&2
  status=1
fi
if ! awk '/^### /{suite=$0} /^=== otter ===/{in_otter=1; saw=0; next} /^=== /{if (in_otter && !saw) missing=1; in_otter=0} in_otter && /^Score \(version [0-9]+\): /{saw=1} END{if (in_otter && !saw) missing=1; exit missing ? 1 : 0}' "$OUT"; then
  echo "error: one or more Otter Octane workloads completed without reporting a score" >&2
  status=1
fi

echo "wrote $OUT" >&2
exit "$status"
