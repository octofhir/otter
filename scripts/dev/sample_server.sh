#!/usr/bin/env bash
# Sample a server's otter-isolate thread while oha drives load against it,
# retrying when `sample` caught too few isolate samples.
# usage: scripts/dev/sample_server.sh <out.txt> <port> <server command...>
set -u
OUT=${1:?out}; PORT=${2:?port}; shift 2
for attempt in 1 2 3 4; do
  "$@" >/dev/null 2>&1 &
  pid=$!
  sleep 1.5
  oha -z 9s -c 50 --no-tui "http://localhost:$PORT/" >/dev/null 2>&1 &
  load=$!
  sleep 2
  sample "$pid" 4 1 -file "$OUT" >/dev/null 2>&1
  wait "$load"
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  n=$(python3 "$(dirname "$0")/isolate_self.py" "$OUT" 1 2>/dev/null | awk '/isolate self samples/ {print $4}')
  if [ "${n:-0}" -gt 1500 ]; then echo "isolate samples $n"; exit 0; fi
done
echo "sampling failed" >&2; exit 1
