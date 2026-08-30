#!/usr/bin/env bash
# Iteration gate. One command, run before every commit that touches the
# interpreter, a JIT tier, or the native ABI.
#
# What it does NOT do: run test262. That belongs to the closing gate of the
# refactor, compared as failing sets against the pre-refactor snapshot.
# While iterating, tier divergence is the real risk and otter-difftest is the
# instrument that catches it.
set -euo pipefail

cd "$(dirname "$0")/.."

step() { printf '\n=== %s ===\n' "$1"; }

step "fmt"
cargo fmt --all

step "clippy"
cargo clippy --all-targets --all-features -- -D warnings

step "unit tests (vm, jit, bytecode)"
cargo test -q -p otter-vm -p otter-jit -p otter-bytecode

# B1 admission boundary. Every artifact that becomes executable is verified in
# release code, so the gate must exercise that boundary in a RELEASE build:
# debug-only coverage would leave overflow checks and codegen differences
# untested on the path adversarial input actually takes.
step "release verifier + adversarial corpus"
cargo test --release -q -p otter-bytecode
cargo test --release -q -p otter-vm --lib code_space
cargo test --release -q -p otter-vm --test snapshot_boundary
cargo test --release -q -p otter-jit --test artifact_agreement

step "difftest: interpreter vs tiers vs gc-stress"
# The full report is megabytes of per-case observations; keep it on disk and
# surface only the verdict. Any mismatch fails the gate.
difftest_report=$(mktemp)
cargo build --release -q -p otter-cli
cargo run --release -q -p otter-difftest -- "$@" >"$difftest_report"
python3 - "$difftest_report" <<'PY'
import json, sys

report = json.load(open(sys.argv[1]))
passed, failed = report["passed"], report["failed"]
print(f"passed={passed} failed={failed}")
if failed:
    for case in report["cases"]:
        if case.get("mismatch"):
            print(f"  MISMATCH {case['case']}: {case['mismatch']}")
    sys.exit(1)
PY
echo "full report: $difftest_report"

step "kernel ledger"
TIERS="${TIERS:-template production-tiered}" bash scripts/cost.sh

cat <<'NOTE'

Gate passed. Append a row to scratchpad/LEDGER.md: hand-written LOC added,
measured % won, and the ratio. If the ratio is not falling across slices, the
substrate is not paying for itself — revert, do not extend.
NOTE
