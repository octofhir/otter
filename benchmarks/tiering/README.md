# Tier-policy calibration

Step 6 replaces universal hotness, bail, PIC-miss, and function-size limits
with one deterministic positive-payoff model. The model predicts compile time
and emitted bytes from immutable function shape, then requires expected saved
execution time to exceed compile cost, previous compile cost, and a code-memory
charge. The executable-code resource ceiling is a separate hard bound.

## Inputs

- `raw/compile-costs.tsv` keeps the original direct compiler anchor plus final
  V8-v7 compile-event aggregates. The final rows include monotonic compile
  duration, queue delay, IR nodes, code bytes, and the function shape recorded
  before compilation.
- `raw/execution-savings.tsv` keeps the four pre-policy interpreter/Template/
  optimizing kernel medians. The scalar numeric loop is the attribution anchor;
  property and element kernels are validation because their committed cold
  effects are not instruction-uniform.
- `raw/capacity-cdf.tsv` records the retained property-program CDF and the
  right-censored generated-call population. Four property programs cover the
  observed distribution. Four call targets failed the DeltaBlue holdout, so the
  existing eight-record storage is retained rather than pretending the
  censored stream proves a smaller capacity.
- `raw/policy-holdouts.tsv` records serial no-event V8-v7 comparisons against
  the Step 5 score and historical code-size census anchors. A score within 3%
  is treated as unchanged; event-capture runs are excluded from wall-time
  comparison. The startup row is a same-binary production-versus-interpreter
  mode control, not a claim about changes before Step 6.
- `raw/octane-final.tsv` records the bounded final Octane sample. The two
  failures are existing harness/runtime conformance gaps and are kept visible;
  they are not converted into policy measurements.

Run `python3 benchmarks/tiering/fit_cost_model.py` from any directory. It uses
only the standard library, prints the exact integer coefficient set in
`crates/otter-vm/src/tier_policy.rs`, derives the backedge terms with a 7%
noise/rounding headroom, selects the capacity CDF, and fails if any chosen
leave-one-workload-out row exceeds its recorded noise envelope.

The fit is intentionally conservative and low-dimensional. It does not claim
that aggregate inline-graph compile time is linear in source bytecode length;
the `ir_nodes` column makes that residual visible. Actual per-function compile
duration is accumulated after every attempt, so repeated compilation becomes
harder to justify without a fixed retry count.
