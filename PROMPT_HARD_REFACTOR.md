# Otter — hard refactor: replace the optimizing tier, the object model and tiering

Goal: V8/JSC-level speed. Measured on the same machine, Otter is **4–7x** slower
than Node on real workloads. This gap is structural; percent-level tuning
inside the current structures will not close it, and that kind of work is
**banned** for this task. Replace subsystems wholesale, modeled on V8/JSC/
SpiderMonkey, and delete what they replace.

There are no users. Breaking any internal contract is allowed and expected:
IR, bytecode, frame layout, object layout, shapes, feedback formats, stubs,
snapshot and cache formats, tests, fixtures. Do not version, bridge, adapt or
flag anything; change the one contract and every producer and consumer
together. Work autonomously. When a session limit hits, schedule a wakeup and
continue.

## Where we are (fixed-work instructions, Otter vs Node, 2026-10-02)

| workload | Otter | Node | gap |
|---|---|---|---|
| ts | 145.0G | 21.0G | 6.9x |
| zlib | 111.4G | 23.1G | 4.8x |
| crypto | 11.6G | 2.93G | 4.0x |
| earley-boyer | 92.8G | measure | — |
| ast_ctor | 5.30G | 3.30G | 1.6x (Bun 2.06G) |
| mega_method | 4.32G | 4.03G | 1.07x (Bun 1.65G) |
| fib | 2.02G | 0.95G | 2.1x (Bun 0.57G) |

## Diagnosis (measured, not guessed)

**1. The Machine tier is the baseline tier with an SSA wrapper.** It is about
57k lines in `crates/otter-jit/src/machine/`.
- Complex ops push their operands through fixed scratch registers
  (`emit_load_allocated_*` into x9–x16) and declare blanket clobber sets
  (`TargetClobberSet::{PropertyLoad, PropertyStore=all caller-saved,
  BindingGuard, InlineMethodGuard, ConstructReceiver, …}`). Register
  allocation then spills around every access. In zlib's hottest function,
  spill reloads (27.5%) plus register moves (20.5%) are about half of all
  samples.
- Guards produce a Boolean (`PropertyShapeProof`, `ElementView` "hit") that a
  later `GuardCondition` or consumer tests, instead of branching straight to
  an eager deopt.
- Named-property access in the optimizing tier is an inline cache with a
  runtime-call CFG (`property_cfg.rs`), not speculation. In ts, 4.8M of these
  calls go to Rust. 2.3M of them hit the shared shape/atom table *in Rust*,
  and 1.2M are primitive receivers.
- One `o.x` costs about 40 instructions. V8 needs about 6:
  `ldr map; cmp; b.ne deopt; ldr field`.
- One unsupported opcode declines the whole function (`JumpIfNullish`,
  `CollectRest`, `ForInKeys`, … in ts). Such functions stay in Template
  forever.
- Compilation costs 25–50 µs per bytecode instruction. In ts it is 2.9 s of
  ~30 s, and 1.9 s of that is recompiles: f1261 is compiled 7 times at
  ~100 ms each, f726 20 times.
- ts breakdown on the isolate thread:
  - JIT code 40%. About 50% of that is property regions
    (`machinePropertyShapeProof` 15%, `PropertyStoreDispatch` 9%,
    megamorphic store/load 13%, slot load 6%). Call entry and trampoline
    together are 15%.
  - VM runtime 29%: `lookup_own_atom`, `jit_load_property_stub`,
    `jit_runtime_resolve_method`, `resolve_property_data_slot`,
    `slot_lookup_at`, `RuntimeCall::bind`,
    `allocate_bytecode_constructor_receiver`.
  - Malloc 7%; GC 5%; compiler 11%.
  - `code_liveness::census_candidate_ids` alone is 3% (repeated whole-heap
    walks).

**2. The object model makes every access check what a hidden class should
prove.**
- Slot location is per object, not per shape: in-object until the first
  overflow, then every slot moves to the slab. Every load therefore branches
  inline vs slab.
- Lookup state (`SLOT_ATTRS_OVERRIDDEN`, `CHAIN_LINK_OPAQUE`,
  `DICTIONARY_COMPATIBLE`, `USED_AS_PROTOTYPE`, extensible) lives in object
  flag bits.
- A shape does not imply the body type, so guards also load the type tag and
  the flags.
- A cell `Value` is already its header's full address (`cage_base | offset`,
  4 GiB-aligned cage). The JIT re-decoded it at every site; this is fixed in
  the WIP, and the rule now applies everywhere.

**3. Tiering recompiles too early and judges profitability by static size.**
- After an optimized deopt the next entry recompiles immediately
  ("hotness survives").
- Profit is estimated from static bytecode size × executions, which
  overestimates large switch-style functions by 20–100x.
- V8 instead resets the interrupt budget on deopt, delays tier-up when an IC
  changes, and counts executed bytecode (`tiering-manager.cc`,
  `deoptimizer.cc` `SetInterruptBudget(kReset)`, `NotifyICChanged`).

## Target architecture (replace; do not patch)

### A. New optimizing tier, Maglev architecture
Read `~/work/engines/v8/src/maglev/` before writing anything:
`maglev-graph-builder.cc`, `maglev-ir.h`, `maglev-regalloc.cc`,
`maglev-known-node-aspects.*`, `maglev-code-generator.cc`, `arm64/`,
`maglev-interpreter-frame-state.*`, `maglev-inlining.cc`.

- **Graph builder.** Build straight from Otter bytecode plus the
  `JitCompileSnapshot` feedback.
  - Abstract interpreter frame state with bytecode register liveness drives
    phis and deopt frames.
  - Every bytecode gets a node; anything not specialized becomes a
    **Generic** node (a runtime/builtin call with a lazy-deopt point).
  - No function-level declines, ever.
- **Speculation nodes with eager deopt that branch straight to the exit:**
  `CheckHeapObject`, `CheckShapes` (a shape switch), `CheckInt32`/
  `CheckNumber`, `CheckBounds`, `CheckValue` (call target).
  - Feedback-driven: mono or poly up to 4 shapes speculates; megamorphic
    uses a stub-cache probe node plus a generic call.
  - Insufficient feedback means a soft deopt.
- **Memory nodes:**
  - `LoadField`/`StoreField` at a static location given by the shape.
  - Transition store: `StoreField` + `StoreShape`.
  - Write barrier node whose slow path saves live registers itself.
  - Typed and dense element loads/stores on `data`/`length` values.
  - Prototype holder loads as constants under code dependencies.
- **Known node aspects.** Known shapes, types and int32-ness flow along
  dominators. Redundant checks fold away, and stores or calls invalidate
  exactly what they may change.
- **Code dependencies instead of runtime checks.**
  - Detach protector, array-index protector, prototype validity, stable
    shapes.
  - Invalidating a dependency deopts dependents (lazy on return from the
    call that invalidated).
- **Register allocation** in the shape of Maglev's straight-forward
  allocator, or regalloc2 with node-declared constraints.
  - Each node declares its inputs (register or any), its result, and its
    *temporaries*.
  - The only emitter scratch is x16/x17/d31 (x86: r11/xmm15).
  - Deferred slow paths save and restore live registers (register
    snapshot). No op clobbers allocatable registers except real calls.
- **Codegen** works directly on allocated registers.
  - Slow paths and deopt exits go out of line.
  - Checks branch to deopt labels with ±1 MiB forms (never `tbz` to far
    labels).
  - Compare+branch fuse.
- **Reuse** the VM-side contracts that are fine:
  - the one-native-frame JS call ABI (x0 ctx, x1 callee, x2 receiver,
    x3 new.target, x4 argc, x8 generation);
  - `DeoptTable` materialization;
  - safepoint tables;
  - code installation and OSR entry;
  - runtime stubs.
- **Cutover.** arm64 first, then x86_64.
  - Build the tier with its own unit tests and the difftest corpus.
  - Cut over in one commit, once it passes every gate and beats the old
    Machine tier on every fixed-work workload.
  - The same commit deletes `crates/otter-jit/src/machine/`. Nothing runs
    both.

### B. Object model as V8 maps (`crates/otter-vm/src/object*`)
- A shape fixes in-object capacity: root key = (prototype, capacity, kind).
  Slot `i < capacity` is in-object; otherwise it is slab word
  `i - capacity`. No migration of all slots on overflow.
- Lookup state is shape identity.
  - Attribute reconfiguration, freeze/seal, preventExtensions and
    prototype-use are transitions, or a move to dictionary mode.
  - Delete is a rollback to the parent shape for the last property;
    otherwise dictionary mode.
  - Chain-opaque objects and host-data objects get distinct roots.
- A shape implies the body type, so the JIT guard is `ldr shape; cmp; b.ne`.
- Constructor instance size uses slack tracking: finalize capacity after N
  instances. Allocation of `new C()` and literals is inline in generated
  code.

### C. Property system shared by all tiers
- One per-site feedback state machine (uninitialized → mono → poly(4) →
  megamorphic) that the interpreter, Template and the new tier all read.
- Megamorphic means an isolate stub cache keyed by (shape, atom), with a
  primary and a secondary table (`src/ic/stub-cache.*`,
  `accessor-assembler.cc` `TryProbeStubCache`).
  - It is probed inline in Template and in the new tier.
  - A hit never enters Rust.
- Primitive receivers (`str.length`, string methods) are handled inline,
  without the full cascade.
- Delete the CacheIR/IC-CFG machinery this replaces.

### D. Tiering manager
- The budget is decremented by executed bytecode: the interpreter at
  dispatch, Template at backedges and returns by bytecode distance.
- Deopt resets the budget; an IC state change raises it
  (`minimum_invocations_after_ic_update`).
- Delete the static-size cost model.
- Measure wall clock as well as instructions: background compilation does
  not reduce instructions, but it does reduce latency.

### E. Calls and runtime overheads
- Call entry and trampoline are 15% of ts JIT time. Match V8's prologue and
  epilogue: frame via `stp`, no redundant pc/call-site stamps, context
  pinned, and the trampoline only for unknown targets.
- Fold code-liveness census into GC marking (bytecode aging), with no
  separate heap walks.
- Cut the cost of `RuntimeCall::bind` and handle scopes on the hot runtime
  paths, and remove malloc from hot runtime paths.

## Milestones (hard targets, fixed-work instructions vs Node)
1. New tier compiles every function of all 7 workloads (no declines). zlib
   ≤ 2.0x and crypto ≤ 2.0x Node. Cutover and deletion of the old Machine
   tier.
2. Object model and property system landed: ts ≤ 3x, ast_ctor ≤ 1.2x,
   mega_method ≤ Bun x1.5.
3. Tiering and calls: fib ≤ 1.3x, ts ≤ 2x; compile ≤ 3% of ts.
Report each milestone with before/after tables, profiles and wall clock.

## State at handoff
HEAD `8302c2dd`. **Uncommitted WIP in `main`** (builds; unit, x86 machine and
clippy tests green before the element rewrite; difftest 93/0 on the binary
before the element rewrite; the other gates have not run):
- Method shape dispatch.
- Template arith feedback with `speculates_int32`.
- LICM hoists values that frame states read: frame states are rewritten to
  the carried params.
- Backedge poll saves registers in its slow path and has no clobbers.
- Cell decode removed (values are addresses): CacheIR prototype holder and
  transition child produce full addresses.
- Short-range `tbz`/`tbnz` to far labels replaced: this fixed every
  `Backend(Relocation)` decline and double compile.
- arm64 element ops rewritten on allocated registers with an empty clobber
  set (`machine/numeric/arm64/element.rs`).

Measured with it: zlib 151.3→111.4G, crypto 12.97→11.6G, mega 5.62→4.32G,
earley 96.1→92.8G, ts 148.0→145.0G. First step: run the full gates on this
WIP and commit it, or fold it into the refactor. Gate script template:
`benchmarks/results/wave-3/gates.sh`.

## Rules (non-negotiable)
- Work only on `main`. No branches, worktrees or stash; never
  checkout/restore over WIP; never touch or commit `PROMPT_*.md`.
- Commits are GPG-signed. **No Co-Authored-By or any AI trailer.** Run
  gates, read the results, then `git commit` in a separate call.
- No mention of other engines anywhere in the repo: code, comments, `//!`
  docs, commits, `benchmarks/`, `docs/`. Engine sources are reading material
  only.
- No flags, env switches, adapters, versions, compatibility shims, bridges;
  no `thread_local`/`static mut`; no bench-specific special cases. No debt
  notes or phase labels in comments or commits. New or changed modules keep
  accurate `//!` docs (`# Contents`, `# Invariants`, `# See also`).
- **Banned:**
  - shaving instructions inside an op whose contract is wrong;
  - adding conditions to a pass to route around a structural problem;
  - per-workload tuning;
  - reporting single-digit wins as progress.
  Every slice replaces a mechanism and deletes the old one.
- Never `cargo test --workspace`; at most one or two runtime `--test`
  binaries. One test262 runner at a time with `--timeout 30000 --jobs 1`. No
  cargo during fixed-work measurement. After big builds run
  `find target -name "*.rcgu.o" -delete` (disk ~95%).
- Results and scripts live in the repo (`benchmarks/results/…`,
  `scripts/dev/…`), never `/tmp`.

## Measure and gate
- Fixed work:
  `python3 scripts/dev/fixed-work.py benchmarks/results/<dir> --engines otter [node bun]`.
  It covers ts, zlib, crypto, fib, mega_method, ast_ctor, earley-boyer;
  compare `*-otter.stdout` against the previous run.
- Wall clock: `hyperfine -N -w 1 -r 5 "target/release/otter run <f>" "node <f>"`.
- Profiles:
  - `sample <pid> <s> -file f.txt` + `scripts/dev/selfprof.py`,
    `bucketprof.py`, `subtreeprof.py`, `parentprof.py`.
  - JIT regions: `DELAY=.. SECS=.. python3 scripts/dev/hotasm.py <js> <dir> N`.
    Artifacts are capped at 1024 objects; aggregate regions by kind.
  - Per-call cost: `scripts/dev/lldb_call_cycle.py`.
- Gates before every commit:
  - `otter-difftest --timeout-ms 60000` (93/0);
  - gc-stress strides 1–16 on the call/property corpora;
  - test262 `language/` (100%) and `built-ins/` (only the 2 known RegExp
    budget failures);
  - `cargo test -p otter-vm --lib`;
  - `cargo test -p otter-jit --lib`;
  - `cargo test -p otter-jit --lib --target x86_64-apple-darwin machine`, or
    the new tier's x86 tests;
  - `cargo clippy --all-targets --all-features -- -D warnings`;
  - fixed-work on all 7 workloads, none worse than the previous commit.
