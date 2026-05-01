# Task 72 — GC core: page heap, scavenger, marking, barriers, handles

## Status

- [ ] **`compressed.rs` — pointer compression: 4 GiB heap cage, `Gc<T>` is `u32` compressed pointer**
- [ ] `header.rs` — `GcHeader` (8 B, atomic flags, forwarding)
- [ ] `page.rs` — 256 KiB aligned `Page` + `PageHeader` + bump alloc
- [ ] `space.rs` — `NewSpace` (semispace) + `OldSpace` (free-list) + `LargeObjectSpace`
- [ ] `scavenger.rs` — Cheney young-gen copy + forwarding
- [ ] `marking.rs` — tri-color worklist + `drain_with_budget` (STW path lit; budget path dormant for Phase 2)
- [ ] `barrier.rs` — generational barrier (load-bearing) + Dijkstra insertion (no-op until Phase 2) + **card-table remembered set (bitmap per page)**
- [ ] **black-allocation flag in alloc fast path** (active when `is_marking == true`)
- [ ] `trace.rs` — `[Option<TraceFn>; 256]` indexed by `type_tag`; **no `Box<dyn>`, no downcast**
- [ ] `handle.rs` — `Gc<T>` (compressed u32), `Local<'gc, T>`, `HandleScope<'gc>`, `GlobalHandle`
- [ ] `heap.rs` — `GcHeap` orchestrator: alloc routing, scavenge trigger, full-GC trigger, cage allocation
- [ ] **`devtools_snapshot.rs` — Chrome DevTools `.heapsnapshot` writer**
- [ ] miri-tested for non-trivial unsafe blocks
- [ ] gates green

## Goal

Stand up a V8/JSC-shaped page-based generational GC in
`crates-next/otter-gc/`. Production-grade from day 1: real
`NonNull<GcHeader>` handles, Cheney scavenger for young, mark-sweep
for old, write barriers wired (generational live; insertion dormant
until Phase 2). Nothing wired into `otter-vm` yet — the crate is
unit-tested against synthetic types.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §2.3
(inheritance ledger, all rows marked "reproduce in Phase 1"), §4.4
(handle survival across moves), §5 (barriers), §6.1 (unsafe
boundary), §8 Phase 1, §9 (file list).

Design reference: `crates/otter-gc/` — read-only. Do **not** import
or paste; rewrite under ADR-0001 §6 conventions.

## Scope

0. **Pointer compression (`compressed.rs`)** — V8 sandbox shape:
   - On startup, reserve a 4 GiB virtual cage with `mmap` /
     `VirtualAlloc2`. All page allocations come from this cage; all
     GC pointers are 32-bit offsets from the cage base.
   - `Gc<T>` is `#[repr(transparent)] struct Gc<T>(u32, PhantomData<*const T>)`.
     `Gc::null()` = `0`. Decompression: `cage_base + (offset as
     usize)`; one register-resident base, no per-load arithmetic
     because `cage_base` is `pub(crate) static`.
   - Heap-side `Value` (the layout used inside `JsObject` slots,
     `JsArray` elements, etc.) becomes a 32-bit tagged compressed
     pointer or 32-bit smi (small integer) when payload fits — V8
     sandbox shape. The mutator-side `Value` (in interpreter
     registers) stays 64-bit NaN-boxed; conversion happens at the
     heap-store / heap-load boundary.
   - Forwarding pointers in scavenger become 32-bit (already fits;
     `u32::MAX` < 4 GiB).
   - Tests: every alloc returns a pointer inside the cage;
     compressing-then-decompressing roundtrips identity; cage
     exhaustion surfaces `OutOfMemory`.

1. **`GcHeader` (`header.rs`)** — `#[repr(C)]`, 8 bytes:
   `type_tag: u8`, `flags: AtomicU8` (mark color 2 b, young 1 b,
   forwarded 1 b, pinned 1 b), `_reserved: u16`, `size_bytes: u32`.
   Forwarding pointer overwrites payload[0..8] when scavenger
   evacuates an object.
2. **`Page` (`page.rs`)** — 256 KiB aligned via `mmap` /
   `VirtualAlloc`. `PageHeader` at base: `space_kind`, `flags`,
   `payload_top` (bump cursor), `survival_age`. Page lookup:
   `addr & !(PAGE_SIZE - 1)`.
3. **Spaces (`space.rs`)** —
   - `NewSpace` — two semispaces; bump alloc in from-space; flip on
     scavenge.
     Default 4 MiB.
   - `OldSpace` — list of pages with free-list per page; survivors
     promoted from young after one scavenge.
   - `LargeObjectSpace` — half-page+ allocations get dedicated
     pages.
4. **Scavenger (`scavenger.rs`)** — Cheney BFS:
   1. Process roots (incl. remembered-set old→young slots).
   2. Each from-space pointer: forwarded? update slot. else copy to
      to-space (or promote on second survival), install forwarding
      pointer, update slot.
   3. Cheney scan to-space linearly; trace each copied object; recurse.
   4. Flip from/to.
5. **Marking (`marking.rs`)** — tri-color worklist `VecDeque<*mut
   GcHeader>`. `drain_full(roots, trace_table)` for STW old-gen
   collect; `drain_with_budget(budget)` exists but is not driven
   yet (Phase 2 wires it).
6. **Barriers (`barrier.rs`)** — V8/JSC-shape from day 1:
   ```text
   write_barrier(parent_header, slot_addr, child) {
       // Generational — load-bearing in Phase 1.
       if parent.is_old() && child.is_young() {
           parent.page().card_table().mark_card(slot_addr);
       }
       // Insertion — dormant until Phase 2 (is_marking == false).
       if marking_state.is_marking && child.is_white() {
           marking_state.shade_gray(child);
       }
   }
   ```
   **Card-table remembered set**: per-page bitmap, 1 bit per
   ~512-byte card. `card_table.mark_card(addr)` is a single
   `(addr >> CARD_SHIFT) & PAGE_MASK` + atomic-or — zero allocation
   on barrier fast path. Scavenger walks dirty cards instead of a
   slot list. Matches V8/JSC.
7. **Trace dispatch (`trace.rs`)** — `pub type TraceFn =
   fn(*const GcHeader, &mut dyn FnMut(*mut *const GcHeader))`.
   Table is `[Option<TraceFn>; 256]` indexed by `type_tag`. One
   indexed load + indirect call per object; no dynamic dispatch
   on `dyn Trait`, no `Box<dyn>`, no downcast.
8. **Handles (`handle.rs`)**:
   ```rust
   #[repr(transparent)]
   pub struct Gc<T> { ptr: NonNull<GcHeader>, _t: PhantomData<*const T> }
   impl<T> Copy for Gc<T> {}
   pub struct HandleScope<'gc> { saved_top: u32, _life: PhantomData<&'gc mut ()> }
   pub struct Local<'gc, T> { idx: u32, _t: PhantomData<&'gc Gc<T>> }
   pub struct GlobalHandle<T> { idx: u32, _t: PhantomData<*const T> }
   ```
   `HandleScope` is RAII; truncates the handle stack to `saved_top`
   on drop. The scavenger walks the handle stack and rewrites
   pointers in place when objects move.
9. **`GcHeap` (`heap.rs`)** — top-level: owns cage + spaces + handle
   stack + global handle table + write barrier + marking state +
   trace table. API:
   ```rust
   pub fn alloc<T: Traceable>(&mut self, value: T) -> Result<Gc<T>, OutOfMemory>;
   pub fn collect_minor(&mut self, roots: &Roots);   // scavenge
   pub fn collect_full(&mut self, roots: &Roots);    // mark + sweep
   pub fn write_barrier<T, U>(&mut self, parent: Gc<T>, slot_addr: *mut Gc<U>, child: Gc<U>);
   ```
   **Black allocation:** `alloc()` checks `marking_state.is_marking`
   in the fast path; if true, the new object's mark bit goes
   straight to black (no need for the marker to re-discover it).
   ~20 LOC; standard since V8 2018.

10. **DevTools heap snapshot (`devtools_snapshot.rs`)** — emit a
    valid Chrome DevTools `.heapsnapshot` JSON:
    - Header + meta arrays (`node_fields`, `node_types`,
      `edge_fields`, `edge_types`, `trace_function_info_fields`).
    - Nodes: one per live object (`type`, `name`, `id`, `self_size`,
      `edge_count`, `trace_node_id`, `detachedness`).
    - Edges: per outgoing reference (`type`, `name_or_index`,
      `to_node`).
    - String table.
    Reuses the `HeapSnapshot` walker from task 74. Output is a
    single file consumable by Chrome DevTools "Memory" panel.

## Tests (in `crates-next/otter-gc/tests/`)

- `compressed_pointers.rs`: alloc; check `Gc<T>` is `u32`-sized;
  decompress-then-recompress identity; cage-base + offset reads
  back the same payload.
- `cage_exhaustion.rs`: fill the 4 GiB cage; subsequent alloc
  returns `OutOfMemory`.
- `cycles.rs`: a synthetic `Cons { car: Value, cdr: Option<Gc<Cons>> }`
  with a cycle; assert `collect_full` reclaims it.
- `scavenger.rs`: alloc 100 young objects, hold 50 via `Local`s,
  scavenge; assert 50 forwarded, 50 reclaimed; subsequent reads
  through the `Local`s see updated pointers.
- `promotion.rs`: alloc 10 young, scavenge, scavenge again; assert
  they live in old space.
- `card_table.rs`: old→young pointer; scavenge without a full GC;
  assert young object survives via dirty card scan; assert the
  card returns to clean after scavenge.
- `black_allocation.rs`: start a marking cycle; alloc one object;
  finish marking without re-entering tracer for the new object;
  assert the new object survives.
- `handle_scope_raii.rs`: nested scopes; outer scope's `Local`s
  remain valid; inner scope's drop on close.
- `trace_table.rs`: register two type tags; allocate one of each;
  full GC traces correctly.
- `devtools_snapshot.rs`: build a small graph; emit
  `.heapsnapshot`; parse it with `serde_json`; assert structural
  validity (header, nodes, edges, strings).
- `miri_smoke.rs`: tiny allocate-and-collect program runnable under
  `cargo +nightly miri test`.

## Out of scope

- OOM cap (task 73), stats / snapshot (task 74), root enumeration
  in `otter-vm` (task 75), per-type migrations (76+), ephemerons
  (80–81).
- Incremental marker driver (task 86).

## Validation gates — production-grade bar

### Functional

- [ ] All ten tests above pass.
- [ ] `cargo fmt --all`, clippy `-D warnings`, `cargo test --workspace` green.

### Memory safety (architecture doc §1.2 NF7)

- [ ] **`cargo +nightly miri test -p otter-gc` green on the full
  test set** (not just `miri_smoke`). Cycle, scavenger, card-table,
  black-allocation, handle-scope, compressed-pointer roundtrip —
  all must pass under miri.
- [ ] **AddressSanitizer build green** —
  `RUSTFLAGS="-Z sanitizer=address" cargo +nightly test -p otter-gc`.
- [ ] **LeakSanitizer build green** —
  `RUSTFLAGS="-Z sanitizer=leak" cargo +nightly test -p otter-gc`.
- [ ] Every `unsafe` block has `// SAFETY:`; every public `unsafe fn`
  has `# Safety` in its docstring (PR review + CI-grep gate).

### Performance baseline (architecture doc §1.2 NF1, NF2)

- [ ] Criterion benches in `crates-next/otter-gc/benches/`:
    - `bench_alloc_young_bump.rs` — single-thread bump alloc; target
      ≤ 10 ns/op.
    - `bench_alloc_with_barrier.rs` — alloc + one pointer-field
      write through write barrier; target ≤ 30 ns/op.
    - `bench_scavenge_4mb.rs` — full 4 MB young-gen scavenge with
      50 % survival; target ≤ 5 ms wall.
    - `bench_collect_full_256mb.rs` — STW full GC at 256 MB live;
      target ≤ 50 ms wall.
- [ ] Numbers checked into
  `docs/new-engine/test262-baseline/gc-bench-baseline.md`.

### Spec / convention hygiene

- [ ] Module docstrings on every `.rs` file per ADR-0001 §6.
- [ ] No `Box<dyn>` on the trace path
  (`grep -rn "dyn TraceFn\|Box<dyn" crates-next/otter-gc/src/` zero
  hits except the visitor closure type `&mut dyn FnMut(…)`).
- [ ] No path-dep on `crates/otter-gc/`.

### Pointer-compression invariants (architecture doc §1.2 NF9)

- [ ] `proptest` corpus: random allocation sequences (1 M ops);
  every returned `Gc<T>` decompresses to a valid in-cage address.
- [ ] Cage-exhaustion test: fill the 4 GiB cage; subsequent
  `alloc()` returns `OutOfMemory`; no UB; cage state recoverable
  by `collect_full`.
- [ ] miri test: scavenger forwards a young object; the `Gc<T>`
  re-read returns the new compressed address; no stale pointer
  use.

## Closing

Gates from [`README.md`](./README.md#closing-a-task), tick 72 in
[70-gc-master-tracker.md](./70-gc-master-tracker.md), delete this
file.
