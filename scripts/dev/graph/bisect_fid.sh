#!/bin/bash
# Narrow a graph-tier failure to one function id: run SCRIPT with only fids
# in [lo, hi) compiled by the graph tier and halve the failing range.
# usage: bisect_fid.sh <otter> <lo> <hi> -- <args...>
set -u
otter=$1; lo=$2; hi=$3; shift 4
# A range fails when either of two runs fails: the failure can depend on
# collection and compile timing.
fails() {
  local lo=$1 hi=$2; shift 2
  for _ in 1 2; do
    OTTER_GRAPH_ONLY="$lo..$hi" timeout 300 "$otter" "$@" >/dev/null 2>&1
    local code=$?
    if [ $code -ne 0 ] && [ $code -ne 124 ]; then return 0; fi
  done
  return 1
}
if ! fails "$lo" "$hi" "$@"; then echo "range $lo..$hi passes"; exit 1; fi
while [ $((hi - lo)) -gt 1 ]; do
  mid=$(((lo + hi) / 2))
  if fails "$lo" "$mid" "$@"; then hi=$mid
  elif fails "$mid" "$hi" "$@"; then lo=$mid
  else echo "failure needs both halves of $lo..$hi"; exit 2; fi
  echo "narrowed to $lo..$hi"
done
echo "fid $lo"
