#!/usr/bin/env bash
#
# Run the standard benchmark suite set. Startup cost (wall + peak RSS) runs
# first and is cheap; it then runs V8 v7 and selected Octane smoke workloads.
# Full Octane/ARES-6/Web Tooling runs are intentionally explicit because they
# can be very long.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

"$HERE/run-startup.sh"
"$HERE/run-v8-v7.sh"
"$HERE/run-octane.sh" richards crypto regexp splay

cat >&2 <<'MSG'

Optional long suites:
  benchmarks/run-octane.sh
  benchmarks/run-ares6.sh
  benchmarks/run-web-tooling.sh --only babel
  benchmarks/run-ejs.sh
MSG
