# Otter Engine Redesign

This is the sole repository-level implementation tracker. Otter is pre-user
and pre-stability: internal APIs, ABI, bytecode, metadata, artifacts, fixtures,
and tests may break whenever that produces the intended final architecture.
Completed slice history and measurements live in git and the ignored
`scratchpad/LEDGER.md`, not in this active plan.

## Objective

Replace every compiled execution path with one typed, target-neutral pipeline:

```text
bytecode + feedback
        |
        v
typed CFG HIR -> Machine IR -> target selection/legalization
        -> regalloc2 -> code + stack maps + deopt maps
```

Quick and optimizing tiers share HIR, Machine IR, target backends, frame
layout, call descriptors, safepoints, deopt reconstruction, dispatch cells,
and artifact schemas. They differ only in optimization budget and tier policy.

## Current state

The active replacement path is:

```text
typed scalar HIR -> Machine IR -> regalloc2 -> AArch64 emitter
```

It already owns:

- straight-line and reducible cyclic scalar CFGs, critical-edge splitting,
  block parameters, edge moves, instruction-exact liveness, and loop OSR;
- Tagged, Int32, Uint32, Float64, and Boolean values, including arbitrary
  tagged parameters, constants, locals, `this`, ordinary returns, tagged
  block parameters, tagged OSR inputs, checked arithmetic, and scalar VM leaves;
- allocator-driven spills, AAPCS64 callee-saved allocation, exact frame sizing,
  fixed leaf ABI operands, and deterministic normalized allocation artifacts;
- `MachineFrameState -> lower_deopt_table -> VM DeoptTable`, shared cold exits,
  exact overflow PCs/windows, and backedge poll before phi moves;
- stack-owned nested `NativeFrame` publication, stable generation cells, and
  generated calls into template or optimizing callees; safepoint-free acyclic
  scalar generations initially publish only their initialized parameter
  prefix, while every cold exit expands the canonical VM window before reentry;
- representation-checked entry and OSR guards with no replay after a started
  operation.

The old optimizing compiler and template emitter remain in the active graph
only for operations and function shapes not yet selected by the replacement
pipeline. They are fallback, not contracts to preserve.

Latest accepted gate: 276 JIT tests, 832 VM tests, all-target/all-feature
Clippy, compile-fail/rooting checks, and 19/19 differential
interpreter/tier/GC-stress cases. Test262 was not run for the engine slices.

## Active work

### 1. Generalize typed HIR and Machine IR beyond numeric functions

Move complete function bodies rather than adding emitter detours:

- calls and constructs through the universal call descriptor and exceptional
  edge model;
- settled property/element loads and stores with explicit dependency tokens,
  guards, barriers, safepoints, and stack maps;
- allocation and reentrant operations with exact tagged roots;
- structured exceptional control flow and multi-frame FrameState chains.

Each operation family must delete its old selector/emitter consumer from the
active path in the same slice. Do not translate new IR back into legacy SSA or
legacy allocation.

### 2. Complete target parity

- implement x86-64 selection, legalization, frame emission, calls, polls,
  safepoints, and deopt exits over the same Machine IR;
- keep JavaScript semantics above target selection;
- make normalized function-local artifacts insensitive to runtime addresses
  and unrelated source edits;
- require cross-target verifier and allocation tests for every shared opcode.

### 3. Perform the atomic compiler switch

Once one complete supported-language boundary exists on both targets:

- make quick and optimizing compilation choose budgets over the same pipeline;
- delete the template compiler, legacy optimizing SSA/regalloc/emitter, their
  interpreter-window ABI, duplicated direct emitters, and obsolete artifacts;
- remove caller-visible callee tier/frame assumptions that are no longer part
  of stable dispatch cells;
- leave the interpreter as the semantic oracle and tier fallback, not as a
  compiled-code ABI.

### 4. Shared semantic optimization

After the atomic switch, add representation propagation, dependency-aware
guard elimination, inlining, GVN, LICM, loop scheduling, and cold outlining.
Quick compilation skips expensive global passes; it does not use a different
backend or value/frame format.

### 5. Scaling slices

Implement in order:

1. typed fields and elements;
2. inline object allocation;
3. inline constructors and virtual objects;
4. OSR at every reducible loop;
5. incremental-GC handshakes using compiled stack maps.

## Required gates

Every substantial slice must prove the affected invariants with focused native
execution before the full gate. Before committing run:

```text
cargo fmt --all
cargo test -p otter-jit
cargo clippy -p otter-jit --all-targets --all-features -- -D warnings
bash scripts/gate.sh
```

Additional correctness gates for the final switch:

- recursive and mutually recursive calls;
- exceptions and reentrant natives;
- moving-GC stress across supported strides;
- nested inline deopt reconstruction;
- OSR entry/exit representation round trips;
- identical normalized artifacts under unrelated source edits;
- no conservative compiled-stack scan and no interpreter-window root ABI.

Performance claims require validated fresh-process A/B measurements. Retired
instructions are primary; wall time is a sanity check. Failed, unavailable, or
unvalidated observations remain visible and are never scoreable.

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

- Parse JS/TS only through the existing AST frontend.
- Keep `otter-gc -> otter-vm -> otter-runtime -> product crates` dependency
  direction and never add `crates-legacy` to the active graph.
- Do not add compatibility readers, schema versions, adapters, replay paths,
  dual writers, or parallel IR/frame/value formats.
- Keep GC roots explicit and allocation-driven; native value building uses
  handle scopes.
- Keep `lib.rs` as a crate map and small glue surface.
- Use focused tests during development and update this plan only with current
  state, next work, and accepted gates.
