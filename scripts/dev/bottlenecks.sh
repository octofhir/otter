#!/usr/bin/env bash
# CPU bottleneck shares (instruction delivery / processing / discarded /
# useful) of the busiest threads of one run, from an xctrace CPU Counters
# recording kept in <work-dir>.
# usage: scripts/dev/bottlenecks.sh <work-dir> <binary> <script.js> [thread-substring]
set -eu
W=${1:?work dir}; B=${2:?binary}; J=${3:?script}; T=${4:-}
mkdir -p "$W"
rm -rf "$W/cc.trace"
xcrun xctrace record --template 'CPU Counters' --time-limit 60s --output "$W/cc.trace" \
  --launch -- "$B" "$J" < /dev/null > /dev/null 2>&1
xcrun xctrace export --input "$W/cc.trace" \
  --xpath '/trace-toc/run[@number="1"]/data/table[@schema="MetricAggregationForThread"]' > "$W/cc.xml"
python3 "$(dirname "$0")/xctrace_bottlenecks.py" "$W/cc.xml" "$T" | head -2
