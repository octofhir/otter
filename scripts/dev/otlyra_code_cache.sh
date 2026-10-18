#!/bin/bash
# Warm-HTTP-cache page loads of one Otlyra binary with and without the code
# cache, interleaved per run.
# usage: scripts/dev/otlyra_code_cache.sh <runs> <url> <otlyra-binary> <config-dir>
# `cold` wipes <config-dir>/code-cache before its load; `kept` loads with what
# the previous load stored. The first load warms both caches and is `warmup`.
RUNS=$1; URL=$2; BIN=$3; CFG=$4
OUT=${OTLYRA_SHOT:-$CFG/shot.png}
mkdir -p "$CFG"
load() {
  OTLYRA_CONFIG_DIR="$CFG" OTLYRA_NO_KEYCHAIN=1 OTLYRA_LOG="warn,page.timing=info" "$BIN" --url "$URL" \
    --screenshot "$OUT" --no-interface --width 1280 --height 900 --scale-factor 1 2>&1 \
    | grep "page.timing" | head -1 | sed 's/\x1b\[[0-9;]*m//g' \
    | grep -oE '(scripts_ms|total_ms)=[0-9.]+' | tr '\n' ' '
}
echo "warmup $(load)"
for r in $(seq 1 "$RUNS"); do
  rm -rf "$CFG/code-cache"
  echo "run$r cold $(load)"
  sleep 1
  echo "run$r kept $(load)"
done
