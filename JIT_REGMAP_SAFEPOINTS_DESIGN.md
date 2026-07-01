# Register-Map Safepoints + Exact-PC Deopt (Item 2)

Working design for the optimizing tier's GC-safepoint / deopt substrate. Goal:
remove the eager box→store→load→unbox of every live value through the tagged
interpreter window `[x19]` at every call/allocating op (the fib ~3× ceiling), by
(a) keeping call-crossing values in stable native homes (spill slots / callee-
saved regs) across the call, (b) building each safepoint's tagged-location map
from only the live **pointer** (Tagged-repr) values at their real homes, and (c)
materializing the interpreter frame **lazily at the cold deopt exit only**.

This is the production-grade prerequisite for item 3 (opt-tier poly `CallMethod`
inline): once the inlined poly bodies compile, their allocating ops (`New`,
literals) must not round-trip live residency through `[x19]`.

## Ground truth (read from current code, branch jit/value-abi-port @ dc3447fb)

Confirmed by reading, not memory:

- **`[x19]` interpreter register window = the GC root array**, holding tagged
  `Value`s. `emit.rs` header (20-38): computed values live UNBOXED in allocator
  regs `x9..x15` / `d0..d5` and the `[sp]` spill area; those hold non-pointers
  (an unboxed int/double, or a boxed-double's bits — also a non-pointer) so they
  need **no** stack map. `[x19]` slots are written only on a deopt restore.
- **Allocator homes** (`regalloc.rs:90`): `Location::Reg(u32)` (GP `x9..x15` /
  FP `d0..d5`) or `Location::Spill(u32)` at `[sp, #s*8]`. All pool GP/FP regs are
  **caller-saved**; the numeric subset makes no calls so needs no prologue save.
- **Safepoints today** (`emit.rs::optimizing_safepoint_records` 195): every
  record is `SafepointRecord::frame_slot_window(id, fs, register_count)` — the
  WHOLE `[x19]` window mapped to `FrameSlot` locations. `TaggedLocation` already
  supports `FrameSlot | MachineRegister | SpillSlot` (`native_abi.rs:565`), but
  only `FrameSlot` is ever constructed.
- **Eager materialize sites** (`emit.rs`): `ArrayPush` 2955, `AllocObjectLiteral`
  3212, `Call` 3264, `CallMethod` 3375 — each calls `emit_frame_materialize`
  (1306: box every live SSA value in the pre-op `DeoptPoint`, `str` to `[x19,
  r*8]`) before the op and `emit_frame_reload` (1325: `ldr` from `[x19]`, unbox
  into the allocator home) after. This round-trip is the tax.
- **Deopt frame state is ALREADY register-precise** (`deopt.rs`): per-guard
  `DeoptPoint { byte_pc, registers: Vec<(reg, SSA NodeId)> }`, pruned by
  `bytecode_liveness` (V8-Maglev-style per-bytecode live sets). `capture_frame_states`
  / `capture_call_resume_states` / `capture_osr_entries` / `capture_deopt_terminators`.
  The deopt exit re-boxes only the live set. **This half is not the gap.**
- **GC consumer** (`runtime_stubs.rs`): `AllocSafepointFrameRoots::visit_extra_roots`
  (274) reads `ctx.frame_slots[loc.index]` as `&mut Value`, traces + lets the
  moving collector rewrite in place. `validate_alloc_safepoint_frame_roots` (207)
  **rejects** MachineRegister/SpillSlot as `UnsupportedLocation`. `RuntimeStubAllocContext`
  exposes `frame_slots` (the `[x19]` window) but NOT the native `[sp]` spill base.

So the gap is exactly: (1) safepoint construction only ever names the whole
`[x19]` window; (2) the emitter therefore must spill all live residency into
`[x19]` before every call; (3) the GC publisher + alloc ctx cannot address
native spill/register homes.

## The mechanism (engine-grounded — fill from JSC DFGOSRExit + V8 Maglev research)

<!-- pending research-agent synthesis: ValueRecovery kinds, RegisterSnapshot,
SafepointTable tagged-slot bits, moving-GC writeback into spill/save area,
unboxed residency across GC call, per-guard snapshot vs compact stream. -->

Decision points to lock after research:

1. Call-crossing homes. All allocator regs are caller-saved → a `Call`/alloc
   `blr` clobbers them. Call-crossing values must be in locations that survive:
   **SpillSlot `[sp]`** (below the callee's sp — untouched) is the natural home;
   callee-saved GP regs `x21..x28` with a prologue save are the register-map
   variant (JSC keeps some values InGPR when the runtime call preserves them).
   First slice: SpillSlot only. MachineRegister map = follow-up once callee-saved
   pool exists.
2. Safepoint tagged set = live values with `Repr::Tagged` at the op's byte-PC,
   mapped to their `Location` (Spill→SpillSlot, the reg case only if it lands in
   a preserved reg). Non-Tagged (Int32/Float64/Bool) residency is **omitted** —
   the GC never sees it; it stays unboxed across the call.
3. Moving-GC writeback: the publisher reads the spill slot as `&mut Value` and
   the collector rewrites the relocated pointer in place (same as the frame-slot
   path today, just addressing `[sp]` instead of `[x19]`). The resumed code
   reloads the pointer from its spill home → sees the updated address.
4. Deopt stays register-precise and LAZY: the cold deopt exit reads each live
   register's home (reg/spill), boxes, stores to `[x19]`, exactly as
   `emit_frame_materialize` does today — but only on the exit path, not eagerly
   before the call.

## Slice plan (each: diff.mjs 24/24 + GC_STRESS=128 + jit/vm tests + fmt/clippy)

- [x] S1-lite (landed `d8191d97`). Frameless self-call: defer the non-arg live
      set's `[x19]` materialization to the cold bail exit; added the filtered
      `emit_frame_materialize_where` helper S2+ reuses. Correct (24/24, GC128,
      jit69/vm652) but **bench-marginal** — the frameless path already pins
      call-crossing values to callee-saved/spill homes, so its live-across set is
      tiny (fib: just `n`). **Finding: item-2's real payoff is at the allocating-
      stub full-materialize sites, and much of it is gated behind item 3** (the
      richards/tree/poly hot bodies are rejected → do not compile yet, so no
      precise safepoint touches them until poly `CallMethod` inline lands).

- [x] S0 (landed). `RuntimeStubAllocContext` gains `spill_slots: *mut u64` +
      `spill_slot_count: u16` (+ `with_spill_area` / `has_spill_slots`);
      `validate_alloc_safepoint_frame_roots` + `AllocSafepointFrameRoots::
      visit_extra_roots` accept + trace `SpillSlot` (read `[spill_slots+idx*8]`
      as `&mut Value` — moving GC rewrites in place); MachineRegister still
      rejected (S3 spills first). Emitter zeroes the two new fields at all 3
      ctx-build sites (opt collection-alloc + 2 baseline) → SpillSlot path dead in
      production, exercised by `spill_slot_safepoint_root_is_traced_and_validated`.
      Struct 64→72B (`abi_records_stay_small` bumped). Zero behavior change:
      diff 24/24, GC128 clean, jit69/vm653, clippy clean.
- [ ] S1. Precise safepoint construction for **one** op class (`Call`): build the
      record from the live Tagged set at their homes instead of `frame_slot_window`;
      thread the spill base into the alloc/reentry ctx at the `Call` lowering;
      replace eager `emit_frame_materialize` before the call with a spill of only
      the call-crossing homes, and move the box→`[x19]` materialize to the deopt
      exit. Measure fib before/after.
- [ ] S2. Extend to `CallMethod`, `AllocObjectLiteral`, `ArrayPush`.
- [ ] S3. MachineRegister map via a callee-saved allocator sub-pool (prologue
      save) for values live across a single call — JSC InGPR recovery analog.
- [ ] S4. Fold the deopt frame-state map and the GC-safepoint map into one
      published per-op descriptor (they share the live-value analysis); one
      `FrameStateId` per safepoint/guard (already present) drives both.

## Verification harness per slice

- `node benchmarks/diff.mjs` == 24/24.
- `RUSTFLAGS="-C debug-assertions=on" cargo build --release -p otter-cli` then
  `OTTER_JIT=1 OTTER_GC_STRESS=128` on nbody / object-shapes / array-ops / fib
  (GC-invariant asserts: `values_ptr_is_current`, tagged-location bounds).
- `cargo test -p otter-jit -p otter-vm`; `cargo fmt --all`; `cargo clippy
  --all-targets --all-features -- -D warnings`.
- Targeted Test262 for any touched semantics; `OTTER_JIT=1 just test262`
  failing-set == JIT-off.
- `node benchmarks/bench.mjs` ×-vs-node before→after (fib is the S1 thermometer).
</content>
</invoke>
