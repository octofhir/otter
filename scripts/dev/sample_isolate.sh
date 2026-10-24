#!/usr/bin/env bash
# Sample a fresh run's otter-isolate thread, retrying when `sample` attached
# before the isolate thread existed (too few isolate samples).
# usage: scripts/dev/sample_isolate.sh <out.txt> <seconds> <delay> <command...>
set -u
OUT=${1:?out}; SECS=${2:?seconds}; DELAY=${3:?delay}; shift 3
for attempt in 1 2 3 4 5; do
  "$@" >/dev/null 2>&1 &
  pid=$!
  sleep "$DELAY"
  sample "$pid" "$SECS" 1 -file "$OUT" >/dev/null 2>&1
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  n=$(python3 "$(dirname "$0")/isolate_self.py" "$OUT" 1 2>/dev/null | awk '/isolate self samples/ {print $4}')
  if [ "${n:-0}" -gt 200 ]; then echo "isolate samples $n"; exit 0; fi
done
echo "sampling failed" >&2; exit 1
