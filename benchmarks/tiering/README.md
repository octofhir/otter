# Tier-policy calibration

`TierWorkModel` admits native compilation from actually entered source-opcode
attempts. A function's immutable `CodeBlock` owns one shared `SourceWork` scalar;
interpreter dispatch and Template execution charge that same allocation. A call
entry, backedge, static loop span, or skipped branch creates no work by itself.
Static function geometry predicts compiler work and code-memory cost only.
Runtime admission uses integer work units; elapsed durations remain diagnostics.

The current actual-work policy has not been performance measured. Historical
holdouts below belong to the preceding policy and cannot establish that the new
admission points improve speed, startup, compilation cost, or memory.

## Current model

Let `N` be source bytecode instructions, `R` register-window slots, `P` formal
parameters, and `A` earlier compiler invocations for this function and tier.
The fixed compiler-work estimates are:

- Template: `334 + 23*N + 4*R + 7*P` source-opcode work units.
- Optimizing Graph: `750 + 88*N + 2*R + 4*P` source-opcode work units.

Predicted code bytes are `560 + 56*N + 16*R` for Template and
`320 + 38*N + 16*R` for Graph. The memory charge is
`ceil(predicted_bytes / 3)` Template work units or
`ceil(predicted_bytes / 5)` Graph work units. Required stable work is
`compiler_work * (A + 1) + memory_work + 1`; compilation requires observed work
to exceed the cost before that final unit. Runtime arithmetic saturates; an
unreachable absolute wakeup target does not wrap.

Resource admission receives `available_code_bytes`, the smaller of the
64 MiB per-isolate headroom and any remaining host `GeneratedCodeBytes` account
limit. The registry derives usage from its existing exact resource leases.
The code owner counts the actual executable mapping reservation, including its
unused tail, and nested code-owned metadata, including reserved vector capacity.
Registration adds its copied dependency box and finalized compilation-root box.
It reserves these payload bytes and a separate persistent `CodeEntryCell` lease
before publication. Invalidated mappings with active frames remain charged until
physical retirement; entry cells remain charged throughout their tombstone
lifetime. The scope excludes shared source owners, GC payload allocations,
directory/bucket storage and allocator bookkeeping. This is retained generated
code backing, not a process RSS cap.

The model declines when `estimated_code_bytes > available_code_bytes`.
After the compiler hook returns, registration independently checks the full
`required_bytes` against the same 64 MiB ceiling and reserves the canonical host
resource leases. An exact resource refusal records that full requirement in the
sole policy state for the function, tier and source feedback epoch. Further
compiler hooks remain blocked while headroom is insufficient; enough headroom
or a feedback epoch change permits reconsideration without inventing additional
source work. The static code estimate predicts compiler/memory work; it does not
replace actual retained resource usage or create a second ledger.

For the synthetic geometry `N=1, R=1, P=0, A=0`, required work is 573 Template
attempts or 916 Graph attempts. These are geometry examples, not universal call
thresholds. Only instructions actually entered on a path accumulate work.

`fit_cost_model.py` converts historical compiler-cost coefficients offline using
12 ns per Template source opcode and 20 ns per Graph source opcode. Each
coefficient is rounded upward: for example, `ceil(4000/12)=334` and
`ceil(270/12)=23`. The historical 4 ns/byte memory charge converts to the exact
divisors 3 and 5. The 12/20 denominators are conservative terms retained from the
prior calibration, not current measurements of every opcode's cost. Neither
these durations nor an entry bonus or whole-body execution estimate is present
in runtime admission.

Run `python3 -B benchmarks/tiering/fit_cost_model.py` from any directory. It uses
only the standard library, prints the current work/code coefficients and clearly
labelled offline calibration diagnostics, checks stored historical holdout rows,
and selects the retained capacity CDF. It performs no engine run, new benchmark,
or leave-one-workload-out refit.

For the direct anchor geometry `N=13, R=12, P=2, A=0`, it prints minimum required
work of 1190 Template attempts and 2129 Graph attempts. This is a static compiler
geometry estimate, conditional on sufficient `available_code_bytes`. The script
also labels predicted code geometry as `estimated_code_bytes`; the old census
contains no exact retained-lease headroom samples and reports that input as
unmeasured. The old census has no exact dynamic opcode traces; its entry
counts and static loop spans are never converted into current observed work or
claims that a historical function would now be admitted.

## Historical inputs and limits

- `raw/compile-costs.tsv` retains the direct compiler anchors and prior V8-v7
  compile-event aggregates. Durations, queue delays, IR nodes, code bytes and
  static function shape remain raw timing/calibration evidence. Aggregate inline
  Graph compilation is not assumed linear in source length; `ir_nodes` exposes
  that residual. These rows predate the current source-work emission overhead.
- `raw/execution-savings.tsv` retains four prior interpreter/Template/Graph
  kernel medians. Their scalar-loop span calculation is historical attribution;
  it does not count current dynamic source-opcode attempts. Property and element
  kernels have committed cold effects and do not establish a uniform opcode cost.
- `raw/capacity-cdf.tsv` retains the property-program CDF and a right-censored
  generated-call population. Four property programs cover the recorded tail.
  Four call targets regressed the historical DeltaBlue holdout; eight remain
  until an uncensored ordinary runtime target histogram supports a new capacity.
- `raw/policy-holdouts.tsv` retains serial no-event comparisons of the preceding
  policy and historical code-size anchors. Scores within the recorded 3% noise
  envelope were treated as unchanged; event-capture runs were excluded from
  timing. The startup row compares modes of the same binary, not source revisions.
  Checking these stored rows is not validation of the current policy.
- `raw/octane-final.tsv` retains the bounded prior Octane sample. Its two existing
  harness/runtime failures stay visible and supply no valid policy measurement.

## Execution and replacement

Feedback changes reset the function's stable work origin. Absolute wakeup targets
use that origin plus required work, preserving work already observed. Earlier
compiler attempts increase the compiler charge without a fixed retry limit.
Generated code retains the exact shared scalar while active or retired; code
object replacement never substitutes a fresh work population for the same source
function.

Interpreter and Template paths count each entered opcode attempt once, including
throws; unreachable suffixes and failed fused preflights are uncharged. A native
opcode that side-exits before completing and is replayed by the interpreter
contributes two actually entered attempts. Multiple
Template operations for one source instruction count once. Successful fused
chains credit their covered instructions; fallback operations credit only those
actually entered. Cold reentrant helpers publish the caller's entered prefix
before invoking JavaScript or native callbacks. Accepted Template inline bodies
charge their own source owners after identity/setup guards pass. Graph execution
does not accrue further admission work; reconstructed interpreter continuations
again count their own real attempts.

A recompiling deopt retires both native tiers for the source function and installed
bodies that actually spliced it. Ordinary generated callers keep their permanent
function cell and follow its current interpreter destination. An active invalidated
native loop leaves at its next bounded poll. The exact splice list belongs to the
code object independently of optional artifacts or exits.

Retraining requires the current model's replacement work budget in actually
dispatched interpreter attempts, plus completion of a fresh activation or two
interpreted backedges to the same loop header within one activation of that epoch.
The function's static length is no longer multiplied by a synthetic execution
count. Interleaved nested headers preserve evidence; a deopt suffix or an older
recursive activation cannot certify a new generation. Entry, OSR, generated
promotion, primitive compile requests, prewarming and inline baking consult the
same per-function gate. Exit history belongs to the source function and site,
shared across its inlining callers.
