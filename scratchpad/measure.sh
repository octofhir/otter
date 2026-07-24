#!/bin/bash
# Retired-instructions matrix for the interpreter tier. Startup is constant
# across before/after so the delta reflects the kernel change.
# Usage: measure.sh [tier]   tier defaults to interpreter
set -euo pipefail
cd /Users/alexanderstreltsov/work/octofhir/otter
B=./target/release/otter-engine-benchmark
TIER="${1:-interpreter}"
S="${SAMPLES:-40}"
declare -a NAME=(method-mono numeric-leaf branch-phi dense-array boxed-double)
declare -a SRC=(method-call-monomorphic numeric-leaf branch-phi dense-array boxed-double-property)
declare -a FN=(engineKernel engineKernel engineKernel engineKernel engineKernel)
declare -a EXP=(500003500000 -700000 -6000000 5234688 4000000)
for i in "${!NAME[@]}"; do
  src="benchmarks/scripts/${SRC[$i]}.js"
  fn="${FN[$i]}"
  exp="${EXP[$i]}"
  # discover expected on first miss by letting the bench validate; we pass known ones
  instr=$( { /usr/bin/time -l "$B" kernel --source "$src" --function "$fn" --expected "$exp" --jit-tier "$TIER" --samples "$S" --warmup 3 >/dev/null; } 2>&1 | awk '/instructions retired/{print $1}')
  printf '%-14s %s\n' "${NAME[$i]}" "${instr:-FAIL}"
done
