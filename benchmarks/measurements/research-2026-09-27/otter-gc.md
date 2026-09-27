# Otter GC subsystem (otter-gc and VM allocation entry points): hot paths, invariants and representation debt

Read-only review. I did not build, run cargo, or touch git state; I used `git diff` only to read the uncommitted work in progress. Instruction counts come from reading emitter and Rust source. Items marked **[est]** are estimates, and **[spec]** marks speculation.

## 1. Components and key files

| Component | Location | Role |
|---|---|---|
| Cage and compressed pointers | `compressed.rs:52,70,134,189`, `Cage` at `:424` | Process-global cage aligned to 4 GiB. The default size is **2 GiB** (`:52`); the doc comment at `:46-51` still says 256 MiB and is stale. `Gc<T>`/`RawGc` are `u32` offsets. `cage_base()` is an **Acquire atomic load on every decompression** (`:134-136`). Page runs live in a `Mutex<BTreeMap>`. `free_page` zeroes all 256 KiB and then calls `MADV_FREE` (`:694-706`). |
| Object header | `header.rs:122-129` | 8 bytes: `tag:u8`, `flags:AtomicU8` (color:2, young, forwarded, pinned, swept, remembered), `u16` reserved, `size:u32`. Mark-color changes use a CAS loop (`:218-232`). |
| Page | `page.rs:60,64,78,137-167` | 256 KiB pages. `PageHeader` holds space kind, bump cursor, allocated/live bytes, `survival_age`, a 64-byte card bitmap that is **dead** (used only by benches; `barrier.rs:36`). `LARGE_OBJECT_THRESHOLD` is about 128 KiB. |
| Spaces | `space.rs:46-177` (`NewSpace`), `:187-476` (`OldSpace`), `:488-555` (`LargeObjectSpace`) | Semispace nursery; non-moving old space with free list, linear allocation area (LAB) and open-page list; multi-page large-object regions. |
| Old free list | `space/free_list.rs:25-29,131-150` | Intrusive links stored in dead filler payloads. Exact classes from 32 to 248 bytes (step 8), then 10 doubling classes, with a nonempty bitmap. Ranges under 32 bytes are never listed (`:79`). |
| Heap orchestrator | `heap.rs:198-310` | Spaces, `TraceTable`, `MarkingState`, `remembered_parents: Vec<RawGc>`, handle and global tables, extra roots, frame-root providers, major-GC budget, GC stress, stats. |
| Scavenger | `scavenger.rs:175-304` | Cheney BFS copy, remembered-parent retrace, promotion worklist, ephemeron and weak passes, drop walk, age bump, flip. |
| Marking | `marking.rs` | `VecDeque` worklist. Mark bits live in object headers. |
| Barriers | `barrier.rs:59-96`; `heap.rs:2488-2590` | Generational barrier (object-granular remembered set) plus a Dijkstra insertion barrier that is effectively dormant. |
| Trace dispatch | `trace.rs:297-316,574-600` | Eight 256-entry function-pointer tables (trace, remembered, ephemeron, drop, finalize, host release, sever, name). Each slot is visited through a `dyn FnMut` call. |
| Roots | `handle.rs`, `root_scope.rs:65-107`, `frame_roots.rs:47-120`, `extra_roots.rs` | Handle stack, global handles, runtime roots, frame providers, `RootScope` slots. |
| Heap image and census | `heap_image.rs:39-41`, `self_contained.rs`, `census.rs` | Old-space page image, **in-process only**; containment audit. |
| Batch old allocation (uncommitted) | `heap/old_batch.rs:31-112`, `tests/old_batch.rs` | See §3.5. |
| VM callers | `upvalue.rs:74-96`, `closure.rs:779-826`, `value_slab.rs:215`, `object/slot_slab.rs:144,185`, `call_ops.rs:1143-1225`, `frame_state.rs:490-557` | Binding cells, closures, element and property slabs, frame spines. |

## 2. Data layouts (bytes)

| Object | Layout | Cell size |
|---|---|---|
| `GcHeader` | tag(1), flags(1), reserved(2), size(4) | 8 |
| `PageHeader` | `repr(C)`; I computed the layout by hand, it is not asserted | 112 → payload 262,032 |
| `UpvalueCellBody` (tag 0x10) | one 8-byte `Value` (`upvalue.rs:52-57`) | **16** (census: 1,774,406,656 B / 110,900,416 cells = 16) |
| `ObjectBody` (0x11) | shape(4) @0, **`values_ptr` absolute \*mut @8**, slab @16, jit_proto @36, 3 inline slots @56 (`object.rs:2093-2109`) | **96** (asserted) |
| `JsClosureBody` (0x23) | `ClosureCallHeader` 24 B (function_id, flags, **`upvalue_base: u64` absolute address** `closure.rs:103`, count, eval_env) + bound_this/new_target 16 + construct 20 + `Option<Gc>` 8 + 3 bools + `Option<Value>` 16 + `named_lookup` | 96-byte payload **[est, computed]** → 104 + 4·captures. Census before the inline tail: exactly 112/closure. |
| `Value` | NaN-box. **Cells hold the full `cage_base\|offset` address** (`value/tag.rs:24-31`). The GC rewrites the low 32 bits in place (`value/mod.rs:1971-1982`). | 8 |
| `MachineAllocationWindow` | 5 pointers + cap (`heap.rs:159-172`) | 48 |
| `JitMachineRootRecord` | previous, root_base, code_object_id, count, safepoint_id (`jit.rs:1182-1202`) | 32 |

Doc drift: `value/mod.rs:35-36` still says "bits 32..48 stay zero". `jit.rs:1455-1465` still says the barrier "marks the card". Both are stale.

## 3. Hot paths

### 3.1 Young allocation in Rust (`alloc_trailing_with_roots`, `heap.rs:1360-1555`)

1. `tenure_all` check.
2. Lazy `trace_table` registration check.
3. Pending-payload root closure is built on the stack.
4. Stress check (`:1420`).
5. `maybe_major_gc` (`:1439`, cheap page-count comparison).
6. Cap check, only if the cap is nonzero.
7. `new_space.alloc`: `Vec` → `Page` → `PageHeader` bump (3 dependent loads). **When the current pages are full, the nursery grows to 64 pages before it ever scavenges** (`space.rs:97`), so the effective nursery is 16 MiB per semispace.
8. When the nursery is full: scavenge (`:1467`); if still no room, overflow into old space.
9. Header write, payload `ptr::write`, zeroing of any trailing storage.
10. Three per-tag stat read-modify-writes (`:1546`).

Cost **[est]**: about 40–70 instructions plus the payload copy.

### 3.2 Old allocation (`alloc_old_with_roots_inner`, `heap.rs:1625-1735`)

Used for binding cells, closures, slabs, shapes, sidecars, Map/Set, promises and generators.

1. Registration check, then cap accounting.
2. `OldSpace::alloc` (`space.rs:231-320`): take the LAB, fall back to free-list `take`, then to open-page bump, then to a standby or new page.
3. **Every LAB split writes a second (filler) header** (`:254-272`) plus the page `allocated_bytes`.
4. Object header and payload write, tail zeroing, `initialize` (`:1702`).
5. **A full `T::trace_slots` barrier scan of the new object**, one `dyn` call per slot (`:1712`).
6. Stats.

Old-path gaps:
- **No stress hook.** `OTTER_GC_STRESS` never forces a collection at an old-allocation boundary.
- **No `maybe_major_gc`.** Only young allocations trigger growth-based major GCs.

Cost **[est]**: about 60–100 instructions for a 16-byte cell.

### 3.3 Inline allocation in generated code

It exists **only for constructor receivers** (`ObjectBody`), in direct construct calls: `arm64/direct_call/receiver_allocation.rs:178-294`, with an x86 twin.

- **Guards (roughly 40–60 instructions):** closure, prototype and shape checks.
- **Space check:**
  - load the marking byte through ctx→thread→cell (3 dependent loads);
  - load the window page and verify the space is `NewFrom`;
  - compare bump + 96 against `PAGE_SIZE`;
  - optional cap check.
- **Initialization:**
  - 6 `stp` stores to zero the cell;
  - header word;
  - shape immediate, prototype, extensible flag, `values_ptr = cell + 56`, 3 × `undefined`, slab length.
- **Publication:**
  - bump cursor and page allocated bytes;
  - tracked bytes;
  - **5 counter read-modify-writes** (3 type rows plus attempts and generated counters);
  - `last_instance` store into the old closure with **no barrier** (it is a weak field that is cleared on every trace, `closure.rs:414`).

Cost **[est]**: about 110–130 instructions, of which about 25 are accounting.

Everything else is a runtime transition: closures, cells, arrays, literals, strings.

### 3.4 Closure creation (Earley `make_closure_value` 127+36 samples)

- **Template:** `transitions.rs:144-178` passes ctx, block, dst, function and a relocated operand-slice pointer to `STUB_JIT_MAKE_CLOSURE`.
- **Stub:** `runtime_ops.rs:131-152` → `make_closure_value` (`function_ops.rs:244-301`):
  1. `function_id_constant`;
  2. SmallVec gather of `frame.upvalue(i)`;
  3. `function_is_arrow` checked 3 times;
  4. `alloc_closure` → `alloc_variable_with_roots_initialized` (**old space**, `closure.rs:811`);
  5. `initialize` copies the captures and writes the absolute `upvalue_base`;
  6. barrier scan of every capture (each capture is an old cell, so each check reads the child header);
  7. `mark_closure_lookup` does another kind lookup and a `with_payload` write.
- **Machine:** goes through `committed_values.rs:607-644`, which **decodes operands from bytecode at runtime** and calls `for_function`.

### 3.5 Frame entry with own captured bindings (the uncommitted work)

- **Generated code:** `jit_initialize_upvalues_stub` (`reentry.rs:1414-1438`) → `jit_initialize_generated_upvalues` (`call_ops.rs:1143-1225`):
  1. `for_function` and `exec_function` lookups;
  2. count validation;
  3. `collect_allocation_roots` (an empty Vec when providers are registered);
  4. `alloc_old_batch_with_roots`;
  5. copies the inherited handles from the closure's tail into the stack frame.
- **Interpreter:** `frame_state.rs:490-557` allocates a **malloc'd `Box<[UpvalueCell]>` per call** (`:61`) in addition to the GC cells.
- **Purpose of `old_batch.rs`:** replace N × (`alloc_upvalue_with_roots` + root closure + cap check) with:
  - one budget check that roots the pending value and inputs;
  - one `OldSpace::alloc` per page-sized chunk;
  - per cell: header, payload, barrier-scan `dyn` call, handle write;
  - stats once per chunk.

  Each cell stays an independent object. The two alternatives were rejected on measurement: grouped owner/index environments cost +14.8% Earley RSS (`2026-09-27-captured-environments.md`), and moving bindings to the nursery cost +6.27% instructions (`2026-09-27-young-bindings.md`). Like the other old paths, the batch has no stress hook and no major-GC trigger.

### 3.6 Upvalue access in generated code

- **Template read** (`binding.rs:257-289`): about 14 instructions [est] and 4 dependent loads (frame → `upvalue_base` → 4-byte cell offset → cage + offset → value), with a hole check.
- **Template write:** the same addressing, then `emit_cell_test` (4), `str`, then the barrier (`binding.rs:108-131`). Total about 22–30 instructions.
- **Machine** (`machine/numeric/arm64.rs:785-826`): the same shape. The inlined `ClosureUpvalue` form reads the closure header's absolute `upvalue_base`.
- **Barrier** (`values.rs:470-518`), with 9 instructions on the young-or-already-remembered-parent exit:
  1. `cbz child`;
  2. 3 dependent loads for the marking byte (**marking is never active while the mutator runs**; see §4);
  3. parent flags byte against young|remembered;
  4. for an old, unrecorded parent: materialize the cage base, load the child flags byte (**a likely cache miss on the child**). That is 17 instructions.
  5. A young child goes to the `WRITE_BARRIER_MUTATING` leaf stub, which pushes onto the `Vec`.

  Binding cells are always old, so the first store of a young value into each cell after every scavenge takes the stub path.

### 3.7 Minor GC (Earley: scavenge 71 samples, `process_slot` 102)

1. Promotion preflight: 2 passes over from-space pages (`scavenger.rs:186-224`).
2. Roots: handle stack, globals, external closures, runtime extra roots, frame providers:
   - interpreter frames;
   - **every published JIT activation's full register window, traced by tag without liveness information** (`frames.rs:279-317`, `active_frame.rs:479-534`);
   - the Machine root-record chain;
   - `RootScope` slots.
3. Retrace remembered parents. **Children of remembered parents are promoted immediately** (`:692`).
4. Cheney scan. **Children of promoted objects are also promoted**, transitively.
5. `finalize_and_drop_dead_from_space` (`:322-351`) walks **every** from-space object, dead or alive, doing 3 table lookups each. That makes the scavenge O(allocated), not O(live).
6. Age bump, then flip.

Per-slot cost in `process_slot` (`:425-469`):
- a `gc_verify_enabled` atomic load;
- an Acquire load of `cage_base`;
- a header young test;
- a page-header `NewTo` load;
- `evacuate`: forwarded check, page age, allocation (**`alloc_in_to` re-probes to-pages starting from index 0**, `space.rs:120-130`), `memcpy`, and 2 atomic read-modify-writes (set forwarded, promote);
- `remember_parent`.

**Page age is never reset.** `survival_age` is zeroed only in `PageHeader::init` (`page.rs:178`) and only incremented afterwards (`scavenger.rs:300`); neither `reset_bump` nor `flip` clears it. Once warm, every recycled page has age ≥ 1, so objects allocated just before a scavenge are tenured on their first survival. The documented "copy once, then promote" behavior does not hold in steady state. I confirmed this by reading the code; I did not measure its effect.

### 3.8 Full GC (STW only; Earley: 244 full vs 626 minor)

- **Trigger:** `major_gc_due` compares (old + large pages) × 256 KiB against a budget of max(16 MiB floor, 1.5× live pages, or 3× when the collection was unproductive), capped at 92% of the cage (`heap.rs:84-112,1904-1987`). It is checked only on young allocation.
- **`mark_phase`** = `start_incremental_mark_phase`:
  1. an embedded scavenge;
  2. **a walk over every object in old, large and from-space to clear the header mark with an atomic `fetch_and`** (`:2060-2076`). This is the likely bulk of the "incremental mark start" 86 samples [spec];
  3. root shading;
  4. `drain_full`, with 2 CAS operations per object.
- **Sweep** (`:2229-2445`):
  - walks every object of every old page;
  - for each dead object: 3 table lookups, the `swept` atomic, and coalescing into a filler;
  - fully dead pages are still walked object by object;
  - then reap, reopen and free-list republication.
- **Why so many full GCs [spec]:** all cells, closures, slabs and sidecars die in old space. Together with the page-age promotion above, nursery garbage becomes major-GC garbage.

## 4. Invariants relied upon

- **One mutator thread, STW collection.** `GcHeap` is `!Send`/`!Sync` (`lib.rs:145-148`). Incremental marking exists as an API (`heap.rs:2047-2131`), but the VM only calls `mark_phase` and `sweep_phase` back to back (`interp/exec.rs:189-213`). So `is_marking` is never observed by the mutator, apart from black allocation during that window.
- **Only young objects move.** Old and large objects never move and there is no compaction. The following depend on that:
  - `ClosureCallHeader.upvalue_base`;
  - NativeFrames that borrow the closure tail (`direct_call.rs:993-994,1050`);
  - global lexical cells embedded in JIT code as absolute addresses (`RelocationTarget::GlobalLexicalCell`);
  - slab `elements_ptr` and `values_ptr` (`value_slab.rs:33-35`);
  - remembered-set offsets and free-list links.
- **Every derived pointer is refreshed inside `trace`,** which mutates the body during tracing (`closure.rs:239-250,404-410`; `ObjectBody` refresh).
- **Rust locals across an allocation must be rooted** (handles, `RootScope`, `external_visit`, `trace_pending_slots`). A pending stack body must never use the tail tracer.
- **Safepoints are allocation calls only.** Generated code spills machine-register roots to mapped slots before any allocating or reentrant call (`native_abi/safepoints.rs:13-16`) and recomputes derived pointers afterwards (`frame.rs:21-22`). The GC reads published register windows and the root-record chain. The PC-keyed `SafepointRecord`s are resolved only for inline-frame deopt (`inline_frames.rs:47`).
- **The JIT allocation window must be revalidated** (space is `NewFrom`, marking byte is clear), because collections flip semispaces (`heap.rs:108-116`).
- **The forwarding offset lives in payload bytes 0..4,** so the minimum cell is 8 bytes. Promotion pages are reserved before the first forwarding write.
- **Swept flag:** dead old objects stay in place, so `FLAG_SWEPT` makes drop idempotent (`header.rs:51-59`).
- **Little-endian plus a 4 GiB-aligned cage in the low 48-bit address window** (`compressed.rs:528-535`) is what makes rewriting only the low 32 bits of a Value slot correct.

## 5. Duplication and representations that could be collapsed

1. **Four upvalue-spine forms:**
   - the closure's trailing 4-byte captures plus an absolute base;
   - a malloc'd interpreter `Box<[UpvalueCell]>` (`frame_state.rs:61`);
   - JIT stack storage filled by a stub;
   - a NativeFrame borrowing the closure's tail.

   Inherited cells are copied on every call in the second and third forms.
2. **Ten allocation entry points with inconsistent policy:** `alloc`, `alloc_with_roots`, `alloc_trailing_with_roots`, `try_alloc_no_collect(_or_return)`, `alloc_old`, `alloc_old_with_roots`, `alloc_old_diagnostic`, `alloc_variable_with_roots(_initialized)`, `alloc_old_batch_with_roots`, plus the JIT window.
   - Stress and major-GC checks happen only on the young path.
   - The barrier scan happens only on the old path.
   - Old versus young placement is decided per type, statically.
3. **Rooting mechanisms:** handle stack, global table, `ExtraRoots`, frame providers, `RootScope`, per-call `external_visit` closures, pending-payload tracing, JIT activation array, Machine root chain, register stack. `root_scope.rs:4-8` itself calls the `*_with_roots` closure twins "ad-hoc".
4. **Barriers:**
   - Rust `write_barrier`, plus 5 heap wrappers (`heap.rs:2488-2590`);
   - ARM64 Template/Machine and x86 emitters;
   - the leaf stub;
   - the post-allocation scan;
   - the dead card table (64 bytes per page, plus APIs and a bench).
5. **Absolute derived pointers stored inside GC bodies** (`values_ptr`, `upvalue_base`, `elements_ptr`). These force old-space placement, a mutating trace, and restore fix-ups.
6. **Two Value widths in the heap:** 8-byte `Value` (so cells are 16 bytes) next to 4-byte `Gc`. Plus a third view: full-address Values in registers.
7. **Accounting at several levels:** page `allocated_bytes`, 3 per-type rows per allocation, `tracked_bytes`, `HeapStats`, JIT counters. The GcHeap field-order comment (`heap.rs:190-196`) claims the cap check shares a cache line with the bump cursor, but the cursor lives in the page header.
8. **Mark state in headers,** so each full GC walks objects twice (clear, then sweep), plus a separate `MarkingState.live_bytes` and page `live_bytes`.

## 6. What would have to change

### (a) Cheaper hot path

- **Make closures movable and young:**
  - derive the capture base as closure + fixed offset instead of storing `upvalue_base`;
  - have NativeFrame hold the closure handle, not a tail address;
  - then add inline bump allocation for closures in generated code.
- **Remove cells for immutable captures** (the compiler proof planned in `closure-captures.md`). Allocate the remaining cells young together with closures. Global lexical cells stay pinned in old space. Moving cells alone regressed because old parents plus immediate promotion defeat the nursery [spec].
- **Fix aging:** reset or age-mark `survival_age` on flip (V8 uses an age mark), or track age per object.
- **Scavenger:**
  - a per-page "has drop/finalize" bit, or a list of droppable young objects, instead of the all-object drop walk;
  - a monomorphic visitor;
  - hoist `gc_verify_enabled` and the cage base out of the per-slot path;
  - plain (non-atomic) header operations under STW;
  - keep a to-space cursor instead of re-probing from page 0.
- **Full GC:**
  - side mark bitmaps (clearing becomes O(pages));
  - skip per-object drop lookups on fully dead pages whose tags need no drop or finalize;
  - lazy sweeping;
  - size-segregated pages for 16-byte cells.
- **Barrier:**
  - drop the marking-byte test, or hold the byte address directly in ctx (1 load instead of 3);
  - test young versus old through page flags derived from the address instead of loading the child header.
- **Allocator bookkeeping:** register trace types once at startup, and batch stats updates per LAB or page refill.

### (b) Portable AOT artifact

Current state: heap images are explicitly in-process only (`heap_image.rs:39-41`).

1. **Heap-stored Values carry the cage prefix.** Either store 4-byte offsets in heap Values, or reserve the cage at a fixed virtual address.
2. **Remove the absolute derived pointers** listed in §5 item 5.
3. **External references are interned by index in install order** (`external_refs.rs`). They need symbolic names.
4. **JIT code:** `artifact/relocation.rs:108-160` already covers:
   - the cage base;
   - runtime stubs;
   - global lexical, string and property-source cells;
   - operand slices;
   - cache tables;
   - direct-call cells.

   Still emitted as plain immediates:
   - shape handles, which are heap offsets (`receiver_allocation.rs:157,217`);
   - function ids;
   - layout offsets tied to one Rust build.

   These need relocations against the snapshot heap and a layout hash.

## 7. Open questions

- Does the CLI run with `max_heap_bytes != 0`? That decides whether the cap paths and the JIT `tracked_bytes` branch are active.
- A full GC triggered by allocation (`maybe_major_gc` → `collect_full`) does not run the VM's ephemeron fixpoint or weak-ref processing; `force_gc` does (`exec.rs:194-212`). Is that semantically correct?
- How are the 86 "mark start" samples split between the clear-mark walk, root shading and the embedded scavenge?
- How much of Earley's 244 full GCs is due to the page-age bug, and how much to direct old-space placement?
- I could not attribute the zlib RSS (795 MB). Candidates [spec]: old-space element slabs, the page-count budget, no compaction, and `MADV_FREE` pages still counted by macOS peak RSS.
- The exact size of `JsClosureBody` is my computation, not a static assert.