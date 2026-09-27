#!/usr/bin/env bash
# Clean native self-sample profile of one script: no --jit-artifacts, no
# --jit-events, so compilation/diagnostic work cannot pollute the profile.
#
# usage: cleanprof.sh <otter-binary> <script.js> <out-dir> [delay] [secs]
# Writes <out-dir>/sample.txt and <out-dir>/self.txt (selfprof.py summary).
# Sample shares are time shares, not retired-instruction shares.
set -euo pipefail
bin=$1 script=$2 out=$3 delay=${4:-1} secs=${5:-10}
mkdir -p "$out"
"$bin" --timeout 0 run "$script" >"$out/stdout.txt" 2>"$out/stderr.txt" &
pid=$!
sleep "$delay"
sample "$pid" "$secs" -file "$out/sample.txt" >/dev/null 2>&1 || true
wait "$pid"
python3 "$(dirname "$0")/selfprof.py" "$out/sample.txt" 80 >"$out/self.txt"
