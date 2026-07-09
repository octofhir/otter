#!/usr/bin/env bash
#
# Run BrowserBench ARES-6. Lower summary milliseconds are better.
# Set ARES6=/path/to/ARES-6 to use an existing checkout.

set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

ARES6_DIR="${ARES6:-$CACHE_DIR/ARES-6}"
KNOWN=(air basic babylon ml)

if [ ! -d "$ARES6_DIR" ]; then
  echo "cloning ARES-6 into $ARES6_DIR" >&2
  git clone https://github.com/bmeurer/ARES-6.git "$ARES6_DIR"
fi

if [ "$#" -gt 0 ]; then
  SELECTED=("$@")
else
  SELECTED=("${KNOWN[@]}")
fi

contains() {
  local needle="$1"
  shift
  local item
  for item in "$@"; do
    [ "$item" = "$needle" ] && return 0
  done
  return 1
}

workload_name() {
  case "$1" in
    air) printf 'Air\n' ;;
    basic) printf 'Basic\n' ;;
    babylon) printf 'Babylon\n' ;;
    ml) printf 'ML\n' ;;
    *) return 1 ;;
  esac
}

workload_benchmark_file() {
  case "$1" in
    air) printf 'air_benchmark.js\n' ;;
    basic) printf 'basic_benchmark.js\n' ;;
    babylon) printf 'babylon_benchmark.js\n' ;;
    ml) printf 'ml_benchmark.js\n' ;;
    *) return 1 ;;
  esac
}

append_required_files() {
  case "$1" in
    air)
      required+=(air_benchmark.js Air/symbols.js Air/tmp_base.js Air/arg.js Air/basic_block.js
        Air/code.js Air/frequented_block.js Air/inst.js Air/opcode.js Air/reg.js
        Air/stack_slot.js Air/tmp.js Air/util.js Air/custom.js Air/liveness.js
        Air/insertion_set.js Air/allocate_stack.js Air/payload-gbemu-executeIteration.js
        Air/payload-imaging-gaussian-blur-gaussianBlur.js Air/payload-airjs-ACLj8C.js
        Air/payload-typescript-scanIdentifier.js Air/benchmark.js)
      ;;
    basic)
      required+=(basic_benchmark.js Basic/ast.js Basic/basic.js Basic/caseless_map.js
        Basic/lexer.js Basic/number.js Basic/parser.js Basic/random.js Basic/state.js
        Basic/util.js Basic/benchmark.js)
      ;;
    babylon)
      required+=(babylon_benchmark.js Babylon/index.js Babylon/benchmark.js)
      ;;
    ml)
      required+=(ml_benchmark.js ml/index.js ml/benchmark.js)
      ;;
    *) return 1 ;;
  esac
}

for key in "${SELECTED[@]}"; do
  if ! contains "$key" "${KNOWN[@]}"; then
    echo "error: unknown ARES-6 workload: $key (valid: ${KNOWN[*]})" >&2
    exit 2
  fi
done

required=(driver.js results.js stats.js glue.js)
for key in "${SELECTED[@]}"; do
  append_required_files "$key"
done

for rel in "${required[@]}"; do
  if [ ! -f "$ARES6_DIR/$rel" ]; then
    echo "error: ARES-6 checkout missing $rel" >&2
    exit 1
  fi
done

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
ENTRY="$WORK/ares6-entry.cjs"
OUT="$RESULTS_DIR/ares6-$(timestamp).log"

to_js_path() {
  local path="$1"
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -m "$path"
  else
    printf '%s\n' "$path"
  fi
}

patch_js() {
  perl -pe 's/(?<![A-Za-z0-9_\$])print\(/console.log(/g' "$1"
}

write_selected_glue() {
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      "driver.addBenchmark(AirBenchmarkRunner);") contains air "${SELECTED[@]}" && printf '%s\n' "$line" ;;
      "driver.addBenchmark(BasicBenchmarkRunner);") contains basic "${SELECTED[@]}" && printf '%s\n' "$line" ;;
      "driver.addBenchmark(BabylonBenchmarkRunner);") contains babylon "${SELECTED[@]}" && printf '%s\n' "$line" ;;
      "driver.addBenchmark(MLBenchmarkRunner);") contains ml "${SELECTED[@]}" && printf '%s\n' "$line" ;;
      *) printf '%s\n' "$line" ;;
    esac
  done < "$ARES6_DIR/glue.js"
}

ARES6_JS_DIR="$(to_js_path "$ARES6_DIR")"

{
  cat <<'JS'
var isInBrowser = false;

const readFileSync = require("node:fs").readFileSync;
const ares6Root = process.env.ARES6_ROOT;

if (!ares6Root) {
  throw new Error("ARES6_ROOT environment variable is required");
}

const ares6Prefix = /[\\/]$/.test(ares6Root) ? ares6Root : `${ares6Root}/`;

function read(source) {
  return readFileSync(ares6Prefix + source, "utf8");
}

globalThis.read = read;

function patchSource(code) {
  return code.replace(/(^|[^A-Za-z0-9_$])print\(/g, "$1console.log(");
}

function makeBenchmarkRunner(sources, name, count = 200) {
  return function runBenchmark() {
    let code = "";
    for (const source of sources) {
      code += patchSource(readFileSync(ares6Prefix + source, "utf8"));
      code += "\n";
    }
    code += `
var results = [];
var benchmark = new ${name}();
var numIterations = ${count};
for (var i = 0; i < numIterations; ++i) {
    var before = currentTime();
    benchmark.runIteration();
    var after = currentTime();
    results.push(after - before);
}
reportResult(results);
`;
    new Function(code).call(globalThis);
  };
}
JS

  for rel in stats.js results.js driver.js; do
    printf '\n// ---- %s ----\n' "$rel"
    patch_js "$ARES6_DIR/$rel"
  done

  for key in "${SELECTED[@]}"; do
    rel="$(workload_benchmark_file "$key")"
    printf '\n// ---- %s ----\n' "$rel"
    patch_js "$ARES6_DIR/$rel"
  done

  printf '\n// ---- selected glue.js ----\n'
  write_selected_glue

  cat <<'JS'
globalThis.reportResult = reportResult;
driver.start(6);
JS
} > "$ENTRY"

OTTER="$(ensure_otter_bin)"

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

run_external_entry() {
  local runtime="$1"
  if ! command -v "$runtime" >/dev/null 2>&1; then
    echo "skip: $runtime not found" >&2
    return 0
  fi
  ARES6_ROOT="$ARES6_JS_DIR" "$runtime" "$ENTRY"
}

run_otter_entry() {
  local timeout="${OTTER_BENCH_TIMEOUT:-0}"
  ARES6_ROOT="$ARES6_JS_DIR" OTTER_JIT="${OTTER_JIT:-1}" "$OTTER" \
    --timeout "$timeout" --allow-read="$ARES6_DIR" --allow-env=ARES6_ROOT run "$ENTRY"
}

status=0
set +e
{
  echo "ares-6: checkout $ARES6_DIR"
  for key in "${SELECTED[@]}"; do
    echo "selected: $(workload_name "$key")"
  done
  run_capped node run_external_entry node
  run_capped bun run_external_entry bun
  run_capped otter run_otter_entry
} 2>&1 | tee "$OUT"
pipe_status=("${PIPESTATUS[@]}")
set -e
if [ "${pipe_status[0]}" -ne 0 ]; then
  status="${pipe_status[0]}"
fi

if grep -Eq '^exit_code: [1-9][0-9]*$' "$OUT"; then
  echo "error: ARES-6 reported benchmark failure" >&2
  status=1
fi
if ! awk '/^=== otter ===/{in_otter=1; next} /^=== /{in_otter=0} in_otter && /^summary:[[:space:]]+[0-9]+([.][0-9]+)?/{found=1} END{exit found ? 0 : 1}' "$OUT"; then
  echo "error: Otter ARES-6 completed without a numeric summary" >&2
  status=1
fi

echo "wrote $OUT" >&2
exit "$status"
