#!/usr/bin/env bash
#
# Run yt-dlp/ejs as a real-world parser/transform benchmark. Lower milliseconds
# are better. Set EJS_DIR=/path/to/ejs to use an existing checkout.

set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

EJS_DIR="${EJS_DIR:-$CACHE_DIR/yt-dlp-ejs}"
ITERATIONS="${EJS_BENCH_ITERATIONS:-3}"

if [ ! -d "$EJS_DIR" ]; then
  echo "cloning yt-dlp/ejs into $EJS_DIR" >&2
  git clone https://github.com/yt-dlp/ejs "$EJS_DIR"
fi

if [ ! -f "$EJS_DIR/package.json" ] || [ ! -f "$EJS_DIR/src/yt/solver/main.ts" ]; then
  echo "error: ejs checkout at $EJS_DIR is missing package.json or solver sources" >&2
  exit 1
fi

if [ ! -d "$EJS_DIR/node_modules" ]; then
  echo "installing yt-dlp/ejs npm dependencies" >&2
  (cd "$EJS_DIR" && npm ci)
fi

PLAYERS_DIR="$EJS_DIR/src/yt/solver/test/players"
if ! find "$PLAYERS_DIR" -type f ! -name '.gitignore' -print -quit | grep -q .; then
  echo "downloading yt-dlp/ejs player fixtures" >&2
  (cd "$EJS_DIR" && node --experimental-strip-types src/yt/solver/test/download.ts)
fi
if ! find "$PLAYERS_DIR" -type f ! -name '.gitignore' -print -quit | grep -q .; then
  echo "error: yt-dlp/ejs player fixtures are missing after download attempt" >&2
  exit 1
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
ENTRY="$EJS_DIR/.otter-ejs-benchmark.ts"
OUT="$RESULTS_DIR/ejs-$(timestamp).log"

cat > "$ENTRY" <<'TS'
import { readFileSync } from "node:fs";

import { getFromPrepared, preprocessPlayer } from "./src/yt/solver/solvers.ts";
import { players, tests } from "./src/yt/solver/test/tests.ts";
import { getCachePath } from "./src/yt/solver/test/utils.ts";

const iterations = Number(process.env.EJS_BENCH_ITERATIONS ?? "3");
let checks = 0;
let playersProcessed = 0;
const started = Date.now();

for (let iteration = 0; iteration < iterations; iteration++) {
  for (const test of tests) {
    for (const variant of test.variants ?? players.keys()) {
      const player = readFileSync(getCachePath(test.player, variant), "utf8");
      const solvers = getFromPrepared(preprocessPlayer(player));
      playersProcessed++;
      for (const mode of ["n", "sig"] as const) {
        for (const step of test[mode] ?? []) {
          const got = solvers[mode]?.(step.input);
          if (got !== step.expected) {
            throw new Error(
              `${test.player}/${variant}/${mode}: expected ${JSON.stringify(step.expected)}, got ${JSON.stringify(got)}`,
            );
          }
          checks++;
        }
      }
    }
  }
}

const elapsed = Date.now() - started;
console.log(`iterations: ${iterations}`);
console.log(`players: ${playersProcessed}`);
console.log(`checks: ${checks}`);
console.log(`summary: ${elapsed} ms`);
TS

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
    awk 'NR <= 120 { print } END { if (NR > 120) print "... truncated failure output (" NR " lines)" }' "$tmp"
    echo "exit_code: $status"
  fi
}

run_node_entry() {
  (cd "$EJS_DIR" && EJS_BENCH_ITERATIONS="$ITERATIONS" node --experimental-strip-types "$ENTRY")
}

run_bun_entry() {
  if ! command -v bun >/dev/null 2>&1; then
    echo "skip: bun not found" >&2
    return 0
  fi
  (cd "$EJS_DIR" && EJS_BENCH_ITERATIONS="$ITERATIONS" bun "$ENTRY")
}

run_otter_entry() {
  local timeout="${OTTER_BENCH_TIMEOUT:-0}"
  (cd "$EJS_DIR" && EJS_BENCH_ITERATIONS="$ITERATIONS" OTTER_JIT="${OTTER_JIT:-1}" "$OTTER" \
    --timeout "$timeout" --allow-read="$EJS_DIR" --allow-env=EJS_BENCH_ITERATIONS run "$ENTRY")
}

status=0
set +e
{
  echo "ejs: checkout $EJS_DIR"
  echo "ejs: iterations $ITERATIONS"
  run_capped node run_node_entry
  run_capped bun run_bun_entry
  run_capped otter run_otter_entry
} 2>&1 | tee "$OUT"
pipe_status=("${PIPESTATUS[@]}")
set -e
if [ "${pipe_status[0]}" -ne 0 ]; then
  status="${pipe_status[0]}"
fi

if grep -Eq '^exit_code: [1-9][0-9]*$' "$OUT"; then
  echo "error: yt-dlp/ejs benchmark reported failure" >&2
  status=1
fi
if ! awk '/^=== otter ===/{in_otter=1; next} /^=== /{in_otter=0} in_otter && /^summary:[[:space:]]+[0-9]+([.][0-9]+)? ms$/{found=1} END{exit found ? 0 : 1}' "$OUT"; then
  echo "error: Otter yt-dlp/ejs completed without a numeric summary" >&2
  status=1
fi

echo "wrote $OUT" >&2
exit "$status"
