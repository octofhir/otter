# Otter jitless interpreter — next structural target is the CALL PATH

Repo: `/Users/alexanderstreltsov/work/octofhir/otter`. Branch main, tree clean
at `b18e37bf`. Read these memory files FIRST (full state + traps):
`dispatch_model_measured_prologue_is_cost` (THE index for this whole line of
work — read it top to bottom), `jitless_node_gap_decomposition`,
`arith_coercion_fusion`, `register_verifier_and_unchecked_window`.

## Mission

Close the `node --jitless` gap on the interpreter with STRUCTURAL VM changes,
not micro-percent tuning. Breaking changes are free — no users, no back-compat,
experimental engine. The user wants a MEGA-FAST engine and has said, repeatedly
and emphatically: **if the architecture is crooked, rewrite it. Do not revert a
working win to avoid touching a hard subsystem (that already happened once with
SMI and the user was furious — I re-did it). Do not hedge about "risk" or
"safety"; there is nothing to protect. Just make it fast, keep the gates green.**

## The method that keeps producing the biggest wins

**Dump node's bytecode and diff it against ours.** This is the single most
productive tool this line of work has had — it finds emitted/executed work that
no interpreter profile can show.
```
node --jitless --print-bytecode --print-bytecode-filter=<fn> file.js   # needs fn CALLED in file
./target/release/otter --dump-bytecode file.js
```
Every big win this session came from it: dead completion-value stores,
LOAD_LOCAL operand copies, SMI immediate fusion.

## What is already done (this session, 5 commits on top of f5f04c73)

- `01516058` per-op dispatch overhead: unchecked top-frame access (frame index
  is verify-once like the register window), `advance_pc_fast` (direct field
  write), arithmetic `op` via `impl Fn` (inlined, no indirect call).
- `d47feb99` CPU-sampler / step-tracer bodies `#[inline(never)]` (they were
  inlined into the hot loop, burning registers when off); budget/profiler/tracer
  hooks folded into one `has_hooks` branch; `record_reductions` wrapping not
  saturating.
- `d3a11da4` statement completion values (`V`) reserved ONLY when observable —
  every `if`/`while`/`for`/`switch` was reserving + storing `V` in every function
  body where the value is unreachable. Plus `-<literal>` folds to a constant.
- `175973ae` a binary operand names its binding register in place (no
  `LoadLocal` copy) when the operator reads both operands before coercing and
  both operands are borrowable-name-or-effect-free-literal.
- `b18e37bf` SMI immediate fusion: 6 opcodes `AddImm SubImm BitwiseAndImm
  LessThanImm EqualImm NotEqualImm`, in the interpreter AND both JIT tiers. The
  optimizing tier absorbs them as ONE-input ops (the `Op::Increment` pattern),
  which is the only way past the `ir/cfg.rs` `pc == index` invariant. Full
  detail + the exact list of tier sites in `dispatch_model_measured_prologue_is_cost`.

Interpreter retired-instructions vs the session-start baseline: branch-phi
-55%, dense-array -42%, numeric-leaf -39%, boxed-double -29%, calls -22..-26%,
method-mono -18%. Gap to `node --jitless` (retired instructions, startup
subtracted): numeric-leaf 3.42x, branch-phi 3.49x, dense-array 3.60x,
boxed-double 4.01x, **method-mono 5.44x**.

## The target: the CALL PATH (method-mono is 5.44x, the worst kernel)

method-mono's gap is NOT op count — `CALL_METHOD_VALUE` is one opcode, same as
Ignition's `GetNamedProperty` + `CallProperty1`. The gap is **the cost of
executing a call**. `sample` of the interpreter running
`benchmarks/scripts/method-call-monomorphic.js` (engineKernel calls
`receiver.apply(index)` 1M times), by self-time:

- **method-IC hit path ~675 samples**: `drive_load_property` 217 +
  `load_own_data_slot_atom` 202 + `FeedbackDirectory::probe_load` 181 +
  `property_ic_site` 75 + `property_atom_for_function` 95 + `is_callable_runtime`
  88. The method IC (`resolve_method_ic` in `crates/otter-vm/src/method_ops/mod.rs`,
  ~line 646) DOES cache the resolved method by receiver shape and hits — but the
  hit path runs through many separate Rust calls (shape check, slot read, atom
  resolve, callable check) where Ignition does it inline in machine code.
  Lever: collapse the monomorphic method-IC hit to a single shape-guard + slot
  read + cached-callee, no per-call atom resolution or function-index lookup
  (`property_atom_for_function` does a `local_function_index` lookup every call).
- **frame build/teardown ~1050 samples**: `push_bytecode_call_frame` 426 +
  `alloc_reg_window` 163 + `_platform_memset_pattern16` 121 +
  `bind_bytecode_call_arguments` 145 + `pop_frame_above` 197, plus
  `do_call_method_value_inner` 488 and `invoke` 268 dispatch overhead.
  - **The register-window zero-fill (memset 121 + part of alloc 163)** is the
    user's repeatedly-flagged "аллоцируем кучу хуеты". `RegisterSegment::allocate`
    in `crates/otter-vm/src/register_stack.rs` does `slots[base..end].fill(undefined)`
    on every call. It exists so a GC safepoint can scan uninitialized window
    slots (the arena is reused, so stale-but-valid Values would be traced as live
    roots → use-after-free). The fix is an init WATERMARK: GC scans the window
    only up to the highest-written slot; params are written by `bind_into`,
    locals advance the watermark as they are first written. A leaf method like
    `apply` (`value + this.bias`, no allocation) never triggers a GC while its
    frame is live, so its scratch slots are never scanned and never need zeroing.
    This is GC-interacting — verify hard under `OTTER_GC_STRESS` and the
    difftest. HotSpot/V8 both do exactly this.
  - **Specialized method entries** (HotSpot idea): a simple method (few params,
    no captures, no arguments object, straight-line) can skip most of frame
    construction. `apply` is exactly this shape.

Order to try: frame-build first (biggest slice ~1050, and the zero-fill is
contained and the user keeps asking for it), then the method-IC hit-path
streamline (~675). Re-`sample` after each to confirm the slice moved.

Other open, smaller levers (from the same node diff):
- Immediate comparisons don't yet fuse with a following `JumpIfTrue/False` in
  the optimizing tier — this is the +6-10% production-tiered gap on branch-phi.
  `fused_numeric_compare_at` in `optimizing/arm64.rs` matches only the register
  compares; extend it + emit flags for `LessThanImm`/`EqualImm`/`NotEqualImm`.
- Immediate `Or`/`Xor`/`Shl`/`Shr` opcodes if a workload wants them.

## Measurement discipline (MANDATORY — the user watches for this)

- Primary metric: RETIRED INSTRUCTIONS on M1 (wall time is 2-3x noisy on this
  box). `/usr/bin/time -l ./bin ... | grep "instructions retired"`.
- Bench binary: `cargo build --release -p otter-benchmark --features engine
  --bin otter-engine-benchmark`. Scripts in the session scratchpad: `measure.sh`
  (interpreter matrix), `measure-tiers.sh` (template + production-tiered — MUST
  run after any bytecode/tier change), and `xeng/` (node-vs-otter drivers).
- `sample <pid>` for profiles. Per-source-line breakdown inside a hot function:
  `grep <fn> prof.txt | awk '{for(i=1;i<=NF;i++)if($i~/^[0-9]+$/){c=$i;break}}{a[$NF]+=c}END{for(k in a)print a[k],k}' | sort -rn`
  — but in a tight loop the per-line attribution SMEARS; trust op-count and
  whole-function self-time over individual lines (a reductions-RMW "hot line"
  measured 506 samples but removing it did nothing).
- Gates at end of each landed batch: `cargo fmt --all`; `cargo test -p otter-vm
  -p otter-compiler`; `cargo test -p otter-jit` (tier artifact goldens);
  `cargo clippy -p otter-bytecode -p otter-vm -p otter-compiler -p otter-jit
  --all-targets --locked -- -D warnings`; `cargo run --release -p otter-difftest
  -- --gc-strides 1,2,4,8,16` (must be 13/13 — this forces the optimizing tier +
  GC stress, it is the real correctness gate for JIT changes); full test262
  (`cargo run --release -p otter-test262 --bin otter-test262 -- run --output
  test262_results/run.json`, must stay 463 fail / 0 crash). Do NOT run
  `cargo test --workspace` (ENOSPC, 80+ bins).
- TRAP that wasted a full test262 run: **never edit a source file while a
  test262 or build job is in flight** — cargo picks up the edit mid-build and
  the result covers an unknown mix. Wait for the job.
- ONE test262 runner at a time. No parallel cargo during a bench.

## Rules (from user memory — non-negotiable)

- NO Co-Authored-By / AI trailers on commits, ever.
- NO feature flags / env toggles / defensive on-off branches. One default path;
  revert via git.
- NO thread_local / static mut / process-global caches (multi-isolate bleed).
  Per-instance -> GC-traced field; per-isolate -> Interpreter field.
- NO simplified algorithms; the optimizing JIT must stay full Maglev-grade.
- Commit correct gated work; NEVER revert a working win to dodge a hard
  subsystem. One Conventional Commit per verified batch, clean tree after.
- Comments timeless (behavior + why), no "Phase X"/"Slice"/task numbers.
- Update `OTTER_PLAN.md` section 1.4 ledger with before/after measurements.
