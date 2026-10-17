#!/bin/bash
# Page-load timings of Otlyra binaries, interleaved per run.
# usage: scripts/dev/otlyra_load.sh <runs> <url> <otlyra-binary>...
# Prints one `page.timing` line per (run, binary); the first run of each
# binary warms the network cache and is printed as `warmup`.
RUNS=$1; URL=$2; shift 2
OUT=${OTLYRA_SHOT:-/private/tmp/claude-501/-Users-alexanderstreltsov-work-octofhir-otter/7890c054-3fb1-4216-b96a-17d44360d476/scratchpad/shot.png}
for r in $(seq 0 "$RUNS"); do
  for bin in "$@"; do
    line=$(OTLYRA_NO_KEYCHAIN=1 OTLYRA_LOG="warn,page.timing=info" "$bin" --url "$URL" \
      --screenshot "$OUT" --no-interface --width 1280 --height 900 --scale-factor 1 2>&1 \
      | grep "page.timing" | tail -1 | sed 's/\x1b\[[0-9;]*m//g')
    tag=$([ "$r" = 0 ] && echo warmup || echo "run$r")
    echo "$tag $(basename "$bin") $(echo "$line" | grep -oE '(scripts_ms|load_event_ms|settle_ms|total_ms|[a-z_]+_ms)=[0-9.]+' | tr '\n' ' ')"
  done
done
