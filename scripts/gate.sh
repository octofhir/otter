#!/usr/bin/env bash
# Engine checks. Use --quick for a focused development loop; the default
# closing gate runs before commits touching the interpreter, JIT, or native ABI.
#
# What it does NOT do: run test262. That belongs to the closing gate of the
# refactor, compared as failing sets against the pre-refactor snapshot.
# While iterating, tier divergence is the real risk and otter-difftest is the
# instrument that catches it.
set -euo pipefail

cd "$(dirname "$0")/.."

step() { printf '\n=== %s ===\n' "$1"; }

usage() {
    cat <<'HELP'
Usage: scripts/gate.sh [difftest arguments...]
       scripts/gate.sh --quick [calls|gc|math|native|properties|all]

Quick mode runs existing focused regressions with incremental debug builds:
  calls  The two constructor moving-GC tests, selected by exact name (default).
  gc     Scavenger, promotion preflight, and remembered-array regressions.
  math   Guarded Math method hits, replacement, and canonical cold completion.
  native Explicit native calls, evaluation order, and moving argument roots.
  properties Named-slot proofs and generated shared-cache loads.
  all    All focused families.

Quick mode defaults OTTER_GC_STRESS to 1 only when it is unset. Existing values
(including 0, 4, 16, or full) are preserved. CARGO_BUILD_TARGET and Cargo runner
configuration select an explicit target; otherwise checks run on the host.

Examples:
  just quick
  OTTER_GC_STRESS=16 just quick calls
  just quick gc
  just quick math
  just quick native
  just quick properties

Quick mode is an iteration check. It does not replace the full gate or required
targeted Test262 evidence. Cold compilation can dominate the first invocation.
HELP
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi

if [[ "${1:-}" == "--quick" ]]; then
    shift
    if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
        usage
        exit 0
    fi
    quick_family="${1:-calls}"
    if (( $# > 1 )); then
        usage >&2
        exit 2
    fi
    case "$quick_family" in
        calls|gc|math|native|properties|all) ;;
        *) usage >&2; exit 2 ;;
    esac
    export OTTER_GC_STRESS="${OTTER_GC_STRESS-1}"

    if [[ "$quick_family" == "gc" || "$quick_family" == "all" ]]; then
        step "quick GC regressions (stress=$OTTER_GC_STRESS)"
        cargo test -q -p otter-gc --lib scavenger::tests
        cargo test -q -p otter-gc --lib promotion_preflight
        cargo test -q -p otter-gc --test promotion_preflight_oom
        cargo test -q -p otter-vm --lib array::elements::remembered_tests
    fi
    if [[ "$quick_family" == "calls" || "$quick_family" == "all" ]]; then
        step "quick constructor regressions (stress=$OTTER_GC_STRESS)"
        for test_name in \
            construct_receiver_and_arguments_survive_reentrant_moving_gc \
            spread_array_survives_receiver_preparation_moving_gc; do
            cargo test -q -p otter-runtime --test jit_machine_direct_call \
                "$test_name" -- --exact
        done
    fi
    if [[ "$quick_family" == "math" || "$quick_family" == "all" ]]; then
        step "quick Math method regressions (stress=$OTTER_GC_STRESS)"
        cargo test -q -p otter-runtime --test math_intrinsic_guards
    fi
    if [[ "$quick_family" == "native" || "$quick_family" == "all" ]]; then
        step "quick explicit native calls (stress=$OTTER_GC_STRESS)"
        cargo test -q -p otter-runtime --test jit_machine_native_call_with_this
    fi
    if [[ "$quick_family" == "properties" || "$quick_family" == "all" ]]; then
        step "quick named properties (stress=$OTTER_GC_STRESS)"
        cargo test -q -p otter-runtime \
            --test jit_machine_generic_properties \
            --test jit_machine_megamorphic_properties
    fi
    printf '\nQuick checks passed; full closing gate remains required.\n'
    exit 0
fi

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

Gate passed. Record hand-written LOC added, measured % won, and the ratio in
the commit message. If the ratio is not falling across slices, the substrate
is not paying for itself — refactor the mechanism, do not extend it.
NOTE
