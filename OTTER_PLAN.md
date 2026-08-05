# Otter Engine Redesign

This is the sole repository-level implementation tracker. Otter is pre-user
and pre-stability: internal APIs, ABI, bytecode, metadata, artifacts, fixtures,
and tests may break whenever that produces the intended final architecture.
There are no compatibility readers, dual writers, legacy modes, or parallel
engine stacks.

## Objective

Replace the interpreter-window-based compiled execution model with one typed,
target-neutral compiler pipeline whose ABI is machine locations plus immutable
metadata:

```text
bytecode + feedback
        |
        v
typed CFG HIR -> Machine IR -> target selection/legalization
        -> instruction sequence -> register allocation
        -> code + stack maps + deopt maps
```

Quick and optimizing compilation share HIR, Machine IR, target backends, frame
layout, call descriptors, safepoints, deopt reconstruction, dispatch cells,
and artifact schemas. They differ only in optimization budget and tier policy.

## Accepted gates

### Compiler walking skeleton

Accepted on 2026-08-05, then deleted rather than landed beside the old
compiler:

- one typed Machine IR selected and encoded real AArch64 and x86-64 code;
- regalloc2 Ion allocation supplied exact final deopt and root locations;
- a tagged root survived an allocating call's caller-saved clobbers;
- unrelated source identity and serialized byte-PC changes left normalized
  function-local artifacts identical;
- x86-64 required no JavaScript-semantic lowering fork.

### Property-slot representation

Accepted on 2026-08-05 and implemented as the first breaking slice:

- every object property slot stores the ordinary 8-byte `Value`;
- `CompressedValue`, heap-number boxes, and JIT slot codecs are deleted;
- generated loads/stores are direct 64-bit operations;
- cell stores retain the precise generational/incremental barrier;
- three inline values preserve the previous 88-byte `ObjectBody` footprint.

The official release memory harness (100,000 iterations, five samples) kept
allocations at 300,004, improved median execution by 1.21% and full-GC time by
4.36%, and increased retained heap by 5,504 bytes (1.37%). VM, JIT, runtime,
and focused GC-stress gates passed.

### Optimizing-backend consolidation

Accepted on 2026-08-05 and implemented before the compiler switch:

- the Cranelift numeric-leaf fork and its backend-specific benchmark contract
  are deleted;
- numeric leaf functions now use the shared optimizing CFG/SSA backend;
- the 20-sample compile median improved from 100,687.5 ns to 70,437.5 ns
  (-30.0%); generated code grew from 252 to 852 bytes;
- the 20-sample production-tiered kernel median improved from 10,138,583 ns to
  2,175,729 ns (-78.5%), with 2.1 million optimizing returns and no deopts.

The shared optimizer is still slower than the current template tier on this
kernel (2.176 ms versus 1.418 ms); backend consolidation removes the much worse
fork but does not close the remaining optimizer/code-generation gap.

## Active implementation

### 1. Atomic compiler and execution-contract switch

Land one complete replacement, not a bridge:

- typed CFG HIR with explicit effects, representations, guards, FrameState,
  dependency tokens, and call descriptors;
- target-neutral Machine IR with blocks, phis, legal machine operations,
  virtual registers, clobbers, and safepoint/deopt annotations;
- complete AArch64 and x86-64 selectors/legalizers over the same Machine IR;
- regalloc2 allocation followed by final stack-map and deopt-map construction;
- universal JS call ABI, native frame walk, stable dispatch cells, and
  generation-independent caller linkage;
- deterministic normalized IR, allocation, code, relocation, stack-map, and
  deopt artifacts.

Landed substrate on 2026-08-05:

- verified target-selected instruction sequences contain dense values, blocks,
  block parameters, physical constraints, explicit clobbers, safepoints, and
  deopt operands without bytecode or interpreter-slot identities;
- every call requires a checked descriptor for argument/result
  representations, effects, clobbers, exceptional transfer, and GC behavior;
- regalloc2 Ion allocates the complete AArch64 and System V x86-64 register
  files, including fixed operands and inserted moves;
- root and deopt values are late allocator uses, and one post-allocation table
  supplies their exact register/spill locations;
- normalized Machine IR and allocation are deterministic, and tests prove a
  tagged root survives the AArch64 caller-saved clobber set.

The repository gate passed with 235 JIT tests, 831 VM tests, and all 17
interpreter/tier/GC-stress differential cases.

First production execution slice landed on 2026-08-05:

- eligible straight-line Number leaves now lower from bytecode into a typed,
  side-effect-free numeric HIR, then into the shared Machine IR;
- eligibility has no fixture-sized arithmetic-count threshold; even a
  one-operation leaf uses the replacement pipeline once feedback proves Number
  inputs;
- regalloc2's exact operand locations drive a new AArch64 emitter directly;
- Number entry guards bail before effects, and result boxing preserves int32,
  double, `NaN`, infinities, and negative zero semantics;
- the installed code object and artifact bundle identify
  `otter-machine-ir numeric-leaf`; the previous optimizing CFG/SSA emitter is
  not entered for eligible leaves;
- one shared post-allocation frame layout now turns allocator spill slots into
  aligned frame bytes and exact stack offsets; the AArch64 emitter executes
  register/spill and spill/spill edits and unwinds the same frame on return or
  bailout;
- native-entry coverage forces 33 simultaneously live numeric values, proves
  real FP spills, validates the returned result, and bails from the spill frame
  without corrupting the VM window or stack;
- the 20-sample compile median is 14,625 ns with 256-byte code, versus 70,437.5
  ns for the consolidated legacy optimizer (-79.2%) and 100,687.5 ns for the
  published baseline (-85.5%);
- the production-tiered kernel median is 1,578,541.5 ns, down from 2,175,729 ns
  (-27.5%), with 2.1 million optimizing returns and zero deopts.

The full repository gate passed with 241 JIT tests, 831 VM tests, all-target
all-feature Clippy, and all 17 interpreter/tier/GC-stress differential cases.

First control-flow slice landed on 2026-08-05:

- the numeric HIR now builds an explicit acyclic CFG from authoritative
  bytecode block boundaries, with typed Number/Boolean values, predecessors,
  successors, block parameters, and per-edge arguments;
- ordered Float64 less-than, both conditional-branch polarities, unconditional
  jumps, and multi-block returns select into the one Machine IR;
- the AArch64 emitter binds block labels and executes regalloc2 edge moves
  before terminators; `NaN` remains unordered and therefore compares false;
- the Machine IR verifier now checks reverse predecessor edges, terminator
  successor counts, and block-parameter/edge-argument representations;
- critical edges carrying arguments are declined until edge splitting lands;
  loops, int32 operations, FrameState, and OSR still use the legacy fallback;
- the current artifact identity is `otter-machine-ir numeric-function` with a
  `machineNumericFunction` code-map region; the obsolete leaf-only identity was
  changed in place;
- a production frontend fixture executes a real diamond and validates its
  merged return. On the same 50-sample harness, compile median fell from
  136,312.5 ns on the parent legacy optimizer to 65,708 ns (-51.8%), with both
  versions producing 468-byte code. The straight-line numeric fixture remains
  256 bytes and measured 13,833 ns versus the previously accepted 14,625 ns.

The full repository gate passed with 243 JIT tests, 831 VM tests, all-target
all-feature Clippy, and all 17 interpreter/tier/GC-stress differential cases.

CFG normalization and loop-SSA substrate landed on 2026-08-05:

- selection splits every critical edge into an explicit Machine IR block, so
  allocator edge moves never execute speculatively on the untaken successor;
- a production frontend fixture validates the split critical-edge path through
  native execution, including the merged Number result;
- numeric HIR now constructs cyclic CFGs, forces typed parameters for values
  live into loop headers, and attaches both preheader and backedge arguments
  after all blocks are lowered;
- regalloc2 accepts the resulting cyclic Machine IR. Native loop publication
  remains explicitly disabled until backedge polling and allocator-driven
  FrameState reconstruction land; this is a safety boundary, not a fallback
  compatibility mode.

The full repository gate passed with 244 JIT tests, 831 VM tests, all-target
all-feature Clippy, and all 17 interpreter/tier/GC-stress differential cases.

Typed loop-value substrate landed on 2026-08-05:

- backward CFG liveness now determines block parameters; dead bytecode
  temporaries from mutually exclusive arms do not become phis or reject SSA;
- Int32 constants, checked add/add-immediate, bitwise-and-immediate, and signed
  less-than/equality-immediate operations have explicit HIR and Machine IR
  identities, with lossless Int32-to-Float64 widening and canonical Int32
  boxing kept separate;
- the exact `branch-phi` bytecode shape now builds typed loop-header and inner
  merge parameters and allocates through regalloc2;
- native publication remains closed for checked integer operations until their
  overflow exits own complete allocator-driven FrameState records. The HIR and
  allocation test asserts this boundary directly.

The full repository gate passed with 245 JIT tests, 831 VM tests, all-target
all-feature Clippy, and all 17 interpreter/tier/GC-stress differential cases.

Machine FrameState lowering substrate landed on 2026-08-05:

- `MachineFrameState` records one register-ordered VM snapshot using only
  Machine values or tagged literal recipes;
- late deopt operands are the sole source of post-regalloc locations;
- one target-neutral lowering unifies GPR, FP, and spill namespaces and emits
  the VM's existing `DeoptTable`, including its schema verification;
- focused coverage proves mixed Int32/Float64 allocator locations and an
  undefined literal reconstruct into one exact frame. Numeric checked exits
  and backedge polls are the next consumers; no emitter-local reconstruction
  format was introduced.

The full repository gate passed with 246 JIT tests, 831 VM tests, all-target
all-feature Clippy, and all 17 interpreter/tier/GC-stress differential cases.

Checked-integer FrameState wiring landed on 2026-08-05:

- backward liveness is now exact at every numeric instruction, not only at
  block entry;
- checked Int32 add and add-immediate nodes own dense frame-state identities
  at their exact byte PCs and keep only live VM registers as late deopt uses;
- the exact `branch-phi` body lowers both overflow exits through regalloc2 into
  complete 12-slot VM frames: three allocated values at `ADD`, two at
  `ADD_IMM`, and tagged `undefined` literals everywhere else;
- native publication remains closed only on backedge polling and cold-exit
  emission. Integer overflow metadata itself no longer depends on the legacy
  optimizer.

The full repository gate passed with 246 JIT tests, 831 VM tests, all-target
all-feature Clippy, and all 17 interpreter/tier/GC-stress differential cases.

Backedge FrameState wiring landed on 2026-08-05:

- every numeric loop backedge, conditional or unconditional, now passes
  through an explicit split-edge Machine IR block;
- that block owns one `BackedgePoll` before its phi moves, so late allocator
  uses describe predecessor values while the exit resumes at the loop header;
- HIR records a complete loop-header VM snapshot for each backedge, masked by
  header liveness. The exact `branch-phi` body lowers its poll into a third
  complete 12-slot frame with two allocated values and tagged `undefined`
  literals everywhere else;
- native publication remains closed until the AArch64 emitter implements the
  poll and shared cold deopt exits. CFG placement, state identity, and
  post-regalloc value locations no longer depend on the legacy optimizer.

The full repository gate passed with 246 JIT tests, 831 VM tests, all-target
all-feature Clippy, and all 17 interpreter/tier/GC-stress differential cases.

Native integer-loop publication landed on 2026-08-05:

- the numeric AArch64 emitter now lowers checked Int32 add/add-immediate,
  bitwise-and-immediate, signed less-than/equality-immediate, and
  `BackedgePoll` directly from allocated Machine IR;
- one post-regalloc frame layout owns spills, the saved `x19` context, and the
  exact generated-stack reservation. Checked overflow and interrupt exits
  branch by dense deopt id into one cold register dump and the existing VM
  `DeoptRuntime`; no emitter-local reconstruction format or replay path was
  added;
- poll clobbers are explicit allocator input. Loop-header values therefore
  remain in exact late-use spill homes until an interrupt deopt completes, and
  the successful fuel-refill call returns before allocator edge/phi moves run;
- native publication no longer has loop/integer-family guards. Focused native
  execution covers both branch-phi arms, multiple iterations, exact `ADD` and
  `ADD_IMM` overflow PCs/windows, forced poll spills, fuel refill, and an
  interrupt exit at the next complete loop-header state;
- the production optimizing selector compiles the exact
  `benchmarks/scripts/branch-phi.js` bytecode shape as
  `otter-machine-ir numeric-function`. The real kernel validated
  `return=-6000000` with seven optimizing entries, zero optimizing OSR entries,
  and zero deopts; a local three-sample median was 2.767 ms versus the
  published 7.366 ms baseline (-62.4%).

The full repository gate passed with 248 JIT tests, 831 VM tests, all-target
all-feature Clippy, and all 17 interpreter/tier/GC-stress differential cases.

Descending integer-loop publication landed on 2026-08-05:

- checked Int32 register subtraction and immediate subtraction now have
  distinct typed HIR and Machine IR operations; AArch64 `subs` overflow exits
  reuse the same allocator-driven `MachineFrameState` and shared VM
  `DeoptRuntime` as checked addition;
- `Increment` selects the existing checked add-immediate operation, while
  immediate inequality has its own Boolean-producing machine operation. No
  bytecode identity reaches the emitter and no second recovery path was added;
- focused native execution covers successful register subtraction,
  subtraction-immediate and increment in a cyclic CFG, plus positive and
  negative overflow with the exact pre-operation VM window and resume PC;
- `benchmarks/scripts/countdown-phi.js` is a real frontend fixture whose loop
  bytecode contains `NE_IMM`, `SUB_IMM`, `INCREMENT`, and a backedge. The
  production optimizing selector test proves the corresponding shape starts
  with `otter-machine-ir numeric-function` and returns `3000000`;
- the validated three-sample production-tiered median was 2.288 ms with seven
  optimizing entries and zero deopts, versus 7.010 ms for template tier
  (-67.4%). Unsupported call and OSR-entry forms remain closed.

The full repository gate passed with 250 JIT tests, 831 VM tests, all-target
all-feature Clippy, and all 17 interpreter/tier/GC-stress differential cases.

Register bitwise loop publication landed on 2026-08-05:

- Int32 `AND`, `OR`, `XOR`, complement, signed left shift, and arithmetic right
  shift now have explicit typed HIR and Machine IR operations and emit directly
  from regalloc2 locations on AArch64;
- variable shifts rely on AArch64's architected low-five-bit count behavior,
  which exactly implements JavaScript's Int32 shift normalization without
  modifying an allocator-owned live right operand;
- focused cyclic-CFG execution covers all six operations, negative and
  greater-than-31 shift counts, allocation, native publication, and canonical
  Int32 return boxing;
- `benchmarks/scripts/bitwise-mix.js` is the real frontend fixture. Its loop
  bytecode contains `SHL`, `SHR`, `BIT_XOR`, `BIT_OR`, `BIT_AND`, `BIT_NOT`,
  checked increment, and a polled backedge; the production selector artifact
  proves it uses `otter-machine-ir numeric-function`;
- the validated three-sample production-tiered median was 3.629 ms with seven
  optimizing entries and zero deopts, versus 18.564 ms for template tier
  (-80.4%). `USHR` remains on fallback because its Uint32 result needs an
  explicit representation decision instead of pretending it is always Int32.

The full repository gate passed with 251 JIT tests, 831 VM tests, all-target
all-feature Clippy, and all 17 interpreter/tier/GC-stress differential cases.

Checked multiply, Uint32, and comparison publication landed on 2026-08-05:

- checked Int32 multiply now stays unboxed through typed HIR and Machine IR.
  The AArch64 widening multiply deopts before committing the destination on
  either signed overflow or a JavaScript negative-zero result, preserving the
  exact pre-operation PC and VM window through the shared deopt runtime;
- logical right shift has an explicit `Uint32` representation from HIR through
  regalloc2, VM `DeoptTable` reconstruction, Float64 widening, and return
  boxing. Values above `i32::MAX` therefore remain exact rather than being
  mislabeled signed integers or forced through an eager boxed detour;
- all six register numeric comparisons select distinct Int32 or Float64
  machine operations and return canonical tagged booleans. Ordered Float64
  comparisons explicitly reject unordered NaN flags while numeric inequality
  remains true for NaN;
- deopt frames now permit several VM slots to read the same immutable machine
  snapshot location. This is the correct final contract for ordinary
  `LoadLocal` aliases and removes a false verifier rejection without adding a
  second recovery representation;
- `benchmarks/scripts/integer-scalar.js` combines checked multiply, Uint32
  loop-header phis, logical shifts, register comparison, bitwise operations,
  checked increment, and a polled backedge. Its production-selector artifact
  proves `otter-machine-ir numeric-function` and native execution returns the
  Node oracle `1725`;
- with a focused OSR threshold of 10, the validated five-sample median was
  3.104 ms with nine optimizing entries and zero deopts, versus 24.778 ms for
  template tier (-87.5%). Unsupported call and OSR-entry forms remain closed.

The full repository gate passed with 255 JIT tests, 831 VM tests, all-target
all-feature Clippy, and all 17 interpreter/tier/GC-stress differential cases.

Typed scalar leaf publication landed on 2026-08-06:

- checked Int32 negation now lowers through typed HIR and Machine IR. AArch64
  deopts on signed overflow and on the JavaScript negative-zero case before
  committing the result, using the existing allocator-driven frame state;
- Number remainder and exponentiation remain unboxed across allocation and use
  one declared `f64, f64 -> f64` leaf ABI. The calls have explicit caller-save
  clobbers and a frame-owned result slot, with no heap argument, tagged-value
  conversion, status pair, local recovery format, or parallel bailout path;
- `ToNumber` over an already numeric value, Int32/Float64 `ToBoolean`, and
  logical-not now lower directly. Float truthiness rejects both zero signs and
  unordered NaN while preserving canonical Boolean results;
- focused native execution covers `INT_MIN`, negative zero, remainder and
  exponentiation edge cases, forced spills across FP leaf calls, several loop
  iterations, and exact interrupt reconstruction before the next iteration;
- `benchmarks/scripts/float-leaf-math.js` is a real frontend fixture combining
  all new families with a checked/polled mixed Int32/Float64 loop. Its artifact
  proves `otter-machine-ir numeric-function`, native execution returns the Node
  oracle `199999`, and ten-sample validation recorded 12 optimizing entries and
  zero deopts. The typed ABI reduced the local production-tiered median from
  19.670 ms with tagged leaf calls to 17.045 ms, within 1.1% of the 16.852 ms
  template result; these dirty-tree measurements are engineering evidence, not
  a published baseline.

The full repository gate passed with 258 JIT tests, 831 VM tests, all-target
all-feature Clippy, and all 17 interpreter/tier/GC-stress differential cases.

Exact ToInt32 bitwise publication landed on 2026-08-06:

- Float64 and Boolean inputs to `&`, `|`, `^`, `~`, `<<`, `>>`, `>>>`, and
  immediate AND now lower through the same typed numeric HIR as existing
  Int32/Uint32 inputs. This removes the parameter/large-constant fallback
  without weakening Number entry guards;
- one declared pure `f64 -> word` VM leaf owns the complete ECMAScript ToInt32
  modulo semantics, including truncation, both zero signs, NaN, infinities,
  values across the signed boundary, and values beyond 32 bits. The AArch64
  ABI uses `d0`/`x0`, the common caller-save clobber set, and the existing
  frame-owned scalar result shuttle; it has no heap, status, exception, or
  alternate reconstruction channel;
- Boolean constants and `NOP` are accepted in numeric CFGs. A typed
  Boolean-to-Int32 conversion preserves the representation boundary instead of
  treating Boolean HIR values as signed integers implicitly;
- focused execution covers the full conversion edge matrix, Uint32 result
  boxing, fractional shift counts, Boolean bitwise input, allocator spills
  across conversion calls, and exact interrupt reconstruction at a loop
  header;
- `benchmarks/scripts/float-bitwise.js` is the real frontend fixture. Its
  artifact contains `Float64ToInt32` and `IntegerLeafResult` under
  `otter-machine-ir numeric-function`, native execution returns the Node oracle
  `120790`, and ten validated samples recorded 13 optimizing entries and zero
  deopts. The dirty-tree production-tiered median was 1.208 ms versus 2.485 ms
  for template tier (-51.4%); this is engineering evidence, not a published
  baseline.

The full repository gate passed with 260 JIT tests, 831 VM tests, all-target
all-feature Clippy, and all 17 interpreter/tier/GC-stress differential cases.

Numeric Machine IR OSR publication landed on 2026-08-06:

- every reducible numeric loop header now owns an explicit `OsrEntry` marker.
  Its immutable metadata aligns live VM-register sources and scalar contracts
  with ordinary late-use Machine IR operands; regalloc2's operand locations
  are the only physical destination map;
- the AArch64 emitter publishes one cold trampoline per marker using the same
  prologue, frame layout, and loop body as normal entry. It decodes exact
  Int32, Uint32, Float64, and Boolean values directly into allocator registers
  or spill homes, then joins after the marker's `Before` edits and before its
  `After` edits;
- a failed representation check writes the loop-header PC and unwinds without
  touching the interpreter window. Successful entry reaches the existing
  checked-operation deopt table and backedge poll, so overflow, interrupt, and
  fuel semantics have no OSR-specific recovery or replay path;
- focused native execution covers successful mid-loop entry, atomic type
  rejection, exact overflow reconstruction, interrupt before phi moves, all
  four scalar contracts, and a 23-value loop header that forces OSR operands
  into allocator spill slots. Artifact coverage records the published OSR
  range and retains the `otter-machine-ir numeric-function` identity;
- the exact `benchmarks/scripts/branch-phi.js` kernel with OSR threshold 10
  validated `return=-6000000`, one optimizing OSR entry, zero optimizing
  deopts, and a five-sample production-tiered median of 2.286 ms versus
  5.386 ms for template (-57.5%). These dirty-tree measurements are engineering
  evidence, not a published baseline.

The full repository gate passed with 264 JIT tests, 831 VM tests, all-target
all-feature Clippy, compile-fail/rooting checks, and all 17
interpreter/tier/GC-stress differential cases.

The remaining legacy optimizer and allocator are fallback for functions whose
HIR/selection slices have not switched. Delete each old consumer as its final
operation family moves; do not adapt old allocation or metadata into the new
pipeline.

In the same switch delete the template compiler, current optimizing SSA path,
interpreter-window compiled ABI, status-return protocol, duplicated direct
emitters, and caller-visible callee tier/frame contracts.

Correctness gates:

- recursive and mutually recursive calls;
- exceptions and reentrant natives;
- moving-GC stress across supported strides;
- nested inline deopt reconstruction;
- OSR entry/exit representation round trips;
- identical function-local artifacts under unrelated source edits;
- no conservative compiled-stack scan and no interpreter-window root ABI.

Performance gates:

- exact instruction counts for settled binding, monomorphic property load and
  store, shape guard, and 0-2 argument monomorphic call;
- current engine `call`, `kernel`, `module`, `jit-compile`, `memory`, and
  `idle-memory` harnesses;
- no benchmark result is scoreable without validation markers and explicit
  tier/GC/runtime-reuse configuration.

### 2. Shared semantic optimization

After the atomic switch, add representation propagation, dependency-aware
guard facts, inlining, GVN, LICM, loop scheduling, and cold outlining to the
shared HIR/Machine IR pipeline. Quick compilation skips expensive global
passes; it does not use a different backend or value/frame format.

### 3. Scaling slices

Implement in this order, each as a vertical behavior/metadata/test/artifact
slice:

1. typed fields and elements;
2. inline object allocation;
3. inline constructors and virtual objects;
4. OSR at every reducible loop;
5. incremental-GC handshakes using compiled stack maps.

No slice may introduce a second IR, call path, frame layout, value format,
deopt schema, or runtime stack.

## Stop conditions

Revise the architecture instead of adding a workaround if any occurs:

1. x86-64 needs a JavaScript-semantic lowering fork.
2. A moving root needs an interpreter window or conservative stack scan.
3. A caller must be recompiled when a callee changes tier or frame size.
4. Typed fields require another deopt, frame, or value schema.
5. Inline allocation cannot share ordinary FrameState and stack-map machinery.
6. Unrelated dead source changes normalized function-local artifacts.
7. Exact post-allocation root/deopt locations require pervasive forced spills.

## Working rules

- The interpreter remains the semantic oracle during replacement, not the
  compiled ABI.
- JavaScript semantics lower once above target selection.
- Target backends own legalization, instruction selection, register classes,
  calling convention details, and encoding only.
- Every substantial change updates runtime behavior, tests, artifacts, and
  contributor docs together.
- Use focused tests during development. Before a commit run `scripts/gate.sh`
  and compare the relevant Test262 failing set when semantics changed.
- Performance claims require fresh-process, randomized A/B measurements with
  medians, dispersion, and validation markers.
