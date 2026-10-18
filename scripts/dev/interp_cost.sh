#!/bin/bash
# Retired instructions per iteration of an interpreter-only probe:
# (instructions at n=1e6 - instructions at n=0) / 1e6.
# Usage: scripts/dev/interp_cost.sh <probe.js> [otter-binary]
set -euo pipefail
probe="$1"; bin="${2:-target/release/otter}"
count() { /usr/bin/time -l "$bin" --interpreter run "$probe" "$1" 2>&1 >/dev/null | awk '/instructions retired/ {print $1}'; }
a=$(count 0); b=$(count 1000000)
echo "$probe: $(( (b - a) / 1000000 )) instructions/iter"
