# Otter value, object, shape and property-IC subsystem: hot paths, duplication and redesign targets

Everything below comes from reading the source. Nothing was built or run. Instruction counts are my own counts of the emitted instruction sequences (marked *est.*). Some conclusions go beyond the code and are marked *speculation*.

## 1. Components and key files

| Component | Location | Role |
|---|---|---|
| `Value` NaN-box | `crates/otter-vm/src/value/tag.rs:1-193`, `value/mod.rs:92` | A `u64` holding the **full** cell address (`cage_base \| offset`), int32 under `NUMBER_TAG`, doubles offset by 2^49 (tag.rs:4-31, 47-62). |
| Pointer funnel | `value/mod.rs:369-400` (`from_cell_offset`), `:1079` `as_raw_gc`, `:621` `as_object` | Decoding truncates to the u32 offset, then re-adds `cage_base()` through an `AtomicPtr` Acquire load (`otter-gc/src/compressed.rs:134-136, 338-347`). |
| `GcHeader` | `otter-gc/src/header.rs:124-129` | 8 bytes: `type_tag`, flags, reserved, `size_bytes`. |
| `ObjectBody` / `JsObject` | `object.rs:729-842`, `:2780` | Ordinary object. JIT offsets are frozen by asserts at `object.rs:2088-2109`. |
| `ExoticSlots` sidecar | `object.rs:923-975` | Rare state: `proto_override`, `dictionary_keys`, `SlotMeta` table, symbol props, host data, wrapper data. |
| Out-of-line slab | `object/slot_slab.rs:53-120, 139-199` | `SlotSlabBody{capacity}` plus trailing words, allocated in **old space** (`otter-gc/src/heap.rs:2628-2656`). |
| Shapes | `object/shape_body.rs:55-83` (`ShapeBody`), `:202-230` (old-space alloc) | Immutable parent-linked nodes. There is **no prototype field**. |
| Shape side tables | `object/shape_runtime.rs:74-89` | `handles_by_id`, `interned_keys: FxHashMap<Box<str>,…>`, `transitions` keyed by `TransitionKey{parent ShapeId, atom, flags, is_accessor}` (:54-66), and `offset_cache` (:334-352). |
| Fast-IC eligibility | `object/shape_cache.rs` | `ShapeCacheMode::{Fast, DictionaryCompatible}`. A delete makes it permanently non-IC. |
| Atoms | `property_atom.rs:51-100` | Per-isolate `NameInterner` (a `Mutex`); u32 `AtomId`. |
| Dictionary keys | `object.rs:1077-1135` `DictKeysBody` | UTF-8 byte arena with an FxHash-by-content bucket table. |
| Per-site IC | `property_ic.rs:103-173` `PropertyIcEntry` (Empty / Polymorphic ≤4 / Megamorphic, terminal); `jit_feedback.rs:164-228` `CodeBlockPropertyFeedback`; `:442-600` `PropertyFeedbackSlot` | PIC capacity is 4 (`tier_policy.rs` `PROFILED_PROPERTY_PIC_CAPACITY`). |
| CacheIR | `cache_ir.rs:43-99` (`CacheOp`, `CacheStub`), `:103-235` `snapshot_for_jit`, `:500-535` `resolve_atom_data_slot` | Interpreter stubs and the JIT DTO (`jit.rs:890` `JitCacheIrOp`, `:984`). |
| Megamorphic tables | `property_cache.rs:59-120` `PropertyLookupCache` (1024 ways), `:222-345` `StoreTransitionCache`; `property_cache/jit.rs` layouts | Isolate-wide `(shape, atom)` table and the add-transition table. |
| Interpreter entry | `interp/dispatch.rs:1095` (`LoadProperty`), `:1219` (`StoreProperty`); `property_dispatch/drivers.rs:55` `drive_load_property`, `:931` `drive_store_property` | |
| JIT miss runtime | `property_dispatch/jit_runtime.rs:81-150, 160+`; `otter-jit/src/entry/runtime_ops/vm_ops.rs:55-118` | `extern "C"` stubs. |
| Template emitters | `otter-jit/src/template/arm64/properties.rs:44-245`; `ic_probe.rs:89-735`; `values.rs:105,329,470` | |
| Machine emitters | `machine/numeric/property_speculation.rs`; `numeric/mod.rs:3420-3700` (selection); `numeric/arm64.rs:2170-2700`; `numeric/arm64/megamorphic_property.rs` | |
| Arrays | `array.rs:70-109` `ArrayBody`, `:356-390` `ArrayExoticSlots`; `array/elements.rs:84-99` | Dense element slab, with named properties kept in a separate sidecar. |

## 2. Data layouts (bytes)

- **`Value`**: 8 bytes. A cell is the full 48-bit address (tag.rs:14). The GC rewrites the low 32 bits in place (tag.rs:23-31).
- **`ObjectBody`**: 88 bytes of payload plus the 8-byte header, so a 96-byte cell (object.rs:2100, 2109).

  | Offset | Field |
  |---|---|
  | 0 | `shape` u32, then 4 bytes padding |
  | 8 | `values_ptr` (cached absolute pointer) |
  | 16 | `slab` u32, then padding |
  | 24 | `dictionary_shape_id` u64 |
  | 32 | `shape_cache_mode` u8 |
  | 36 | `jit_proto` u32 |
  | 40 / 41 / 42 | `extensible`, `slot_attrs_overridden`, `chain_link_opaque` |
  | 44 | `dictionary_layout` u32 |
  | 48 | `exotic` (8 bytes, of which 4 are pad, :882-887) |
  | 56 | `inline_values[3]` |
  | 80 | `slab_len` u16 |

  Only 24 of the 88 bytes are property payload. `values_ptr`, `dictionary_shape_id` and the padding account for about 24 bytes that a V8-style layout would not have.
- **Four or more properties**: the whole slab moves to an old-space `SlotSlabBody` of capacity `max(needed, 2·cap, 6)` (object.rs:2813-2817, 3135). The 3 inline words are abandoned. A 4-field object costs 96 + (8+8+48) = **160 B** (*est.*). V8 would use roughly 12 + 4·4 = 28 B.
- **`ShapeBody`** (repr C): `id` u64 @0, `parent` u32 @8, `transition_key` @12, `transition_atom` @16, `property_count` @20, `own_offset` @24, `own_flags` u8 @28, `own_is_accessor` @29. That is 32 B, or a 40 B cell (*est.* from field order). Side-table entries add more per shape.
- **`AtomOwnPropertyHit`**: 24 B (object.rs:654-669).
  - `PropertyLookupCache::Entry` is 40 B × 1024 (property_cache.rs:59-73).
  - `StoreTransitionJitEntry` is about 96 B × 1024 (:238-248), plus a parallel `ways` array.
- **Per-site feedback**: one `Box<CodeBlockPropertyFeedback>` is allocated eagerly for every `LoadProperty`, `StoreProperty` or `CallMethodValue` instruction (jit_feedback.rs:417-431). Each box holds a `Mutex`, a `PropertyIcEntry<CacheStub>` (an inline `SmallVec<[CacheStub;4]>`; each `CacheStub` is four SmallVecs, cache_ir.rs:88-99) and 8 `AtomicU64` counters, of which only 4 are used per kind. *Est.* several hundred bytes per site. This was not measured and could contribute to TS RSS (*speculation*).
- **`ArrayBody`**: not repr(C), asserted ≤64 B (array.rs:431). Named properties sit in `IndexMap<String,Value>` inside the sidecar (array.rs:363-386), outside the shape system.
- **Element slab**: header 24 B (elements.rs:84-99).

## 3. Hot paths

### 3a. Monomorphic own-data load

**Interpreter** (dispatch.rs:1095 → drivers.rs:55-79):
1. `context.property_atom` decodes the atom.
2. `function.property_feedback_at(pc)` indexes `typed_slots`, matches the enum and dereferences the Box (executable.rs:629; jit_feedback.rs:976).
3. `read_register`, then `as_object`: a cell test, a `cage_base` Acquire load and a header tag compare.
4. `mono_load_own_data_hit` matches the entry, checks `len==1` and pattern-matches the stub ops and hits (jit_feedback.rs:578).
5. `load_own_data_slot_by_shape` (object.rs:4025-4046): a second `cage_base` load, compares `body.shape==hit.shape` plus 4 flag bytes, then reads `*values_ptr.add(slot)`.
6. `slot.record_hit()` does an **atomic `fetch_add`** on every hit (jit_feedback.rs:518-523).
7. `advance_pc`, then `write_register`.

*Est.* 60-100 instructions plus dispatch. No allocation or barrier.

**Template** (properties.rs:81-102 → ic_probe.rs:380-525):

| Step | Emitted | Instrs |
|---|---|---|
| Load receiver | `ldr` from frame | 1 |
| Cell test (values.rs:105) | `movz`/`movk` mask, `tst`, `b.ne` | 4 |
| Re-decompress | `mov w12,w9`, `movz`/`movk` cage base, `add` | 3-4 |
| Type tag | `ldrb`, `cmp`, `b.ne` | 3 |
| Ordinary-state guard (:89-126) | 3 × (`ldrb` + `cbnz`) | 6 |
| `GuardShape` → `emit_check_shape` (:260) | runs the state guard **again**, then `ldr shape`, `cbz`, imm, `cmp`, `b.ne` | 12 |
| `LoadField`: `emit_slab_base` (values.rs:329) | inline-vs-spilled branch on the slab handle, then `cbz`, `ldr` | 6 |
| Store result | store to `dst` | 1-2 |

Total ≈ **38** (*est.*). There is no runtime call on a hit. A miss calls `STUB_JIT_LOAD_PROPERTY` (properties.rs:117-145).

**Machine**, when speculation is admitted (property_speculation.rs:46-72, 202-243):
- `PropertyShapeProof` (arm64.rs:2505-2537): header decode + state guard + identity check + boolean materialize/store, ≈27.
- `GuardCondition` exits to deopt.
- `PropertySlotLoad` (arm64.rs:2539-2564): decodes the header **again** (`mov w12,w9`; cage `movz`/`movk`; `add`), runs `emit_slab_base` and `ldr`, ≈10.

Total ≈ **40** (*est.*). GVN can common the proof and LICM can hoist it (property_speculation.rs:20-24). A `ShapeGuard` exit permanently disables re-speculation at that site (:17-19).

### 3b. Polymorphic load (2-4 shapes)

- **Interpreter**: `probe_load` iterates the stubs through `run_load` (cache_ir.rs:304-321). Own-data stubs compare the handle. Prototype stubs run `run_guards`, where `GuardShapeId` dereferences the shape body for its u64 id (object.rs:3677-3687) and `LoadPrototype` calls `prototype()`, which checks the sidecar first (object.rs:2494-2503) and then `supports_fast_property_ic`, which dereferences the sidecar for `string_data` (shape_cache.rs; object.rs:2546).
- **Template**: one header decode, then per program the state guard (6) + shape check + field.
- **Machine**: own-data cases share `PropertyPolymorphicLoad` (one decode, compare chain; arm64.rs:2376-2435). A miss goes to the committed cold call.

### 3c. Prototype-chain load (holder = direct prototype)

- The CacheIR is `GuardShape r; LoadPrototype; GuardShape p; GuardAtomSlot p; LoadField p` (cache_ir.rs:238-254; property_ic.rs tests). Only depth ≤1 can be cached (cache_ir.rs:486-491). Deeper holders always take the full walk.
- **Template** ≈55-60 (*est.*). `LoadPrototype` is `ldr jit_proto`, `cbz`, cage materialize, `add`, tag check and the state guard again (ic_probe.rs:480-490).
- **Machine**: never speculated. **Every CacheIR op becomes its own Machine instruction** (numeric/mod.rs:3620-3700). Each one:
  1. loads an incoming boolean and `cbz`;
  2. re-decodes the receiver header;
  3. re-runs the state guard;
  4. writes a boolean back to its location (arm64.rs:2170-2375).

  Five ops come to ≈100+ instructions (*est.*) plus select/or glue.

### 3d. Megamorphic load

- **Interpreter** (drivers.rs:80-126):
  1. `probe_load` over an empty PIC.
  2. Re-read the register.
  3. `migrate_slow_to_fast` walks prototype depths 0..8 with O(d²) hops plus dictionary checks on **every** access (interp/shapes.rs:312-338).
  4. `resolve_property_data_slot` (property_cache.rs:387-400): `shape()`, a `shape_id` dereference, two 64-bit multiplies and a compare, then `load_own_data_slot_atom`.
  5. On an `Unknown` entry: `resolve_atom_data_slot`, which walks the shape chain O(depth) twice (offset in shape_body.rs:293-309, attributes in `shape_slot_attrs`, object.rs:2363-2372).
- **Template**: the table is **never probed**. A megamorphic site has no programs, so every access branches to the stub (ic_probe.rs:395-398). The stub path is:
  1. `jit_load_property_stub`;
  2. `inline_frames::decode`, then `runtime_call`, then `with_inline_activations` (vm_ops.rs:55-80);
  3. `named_property_site` re-decodes the instruction (jit_runtime.rs:29-73);
  4. `HandleScopeFrame::enter` and `scoped_value`, then the table resolve (jit_runtime.rs:81-110).
- **Machine**: an inline probe of ≈70 instructions (*est.*) (megamorphic_property.rs:22-150). It dereferences the shape cell for the u64 id, materializes 64-bit multipliers with `movz`/`movk` (4 each), does two `mul`, indexes the table, checks atom, hops, is_data and holder shape, bounds-checks inline vs slab, then loads. A miss goes to the cold stub.

### 3e. Stores

- **Interpreter**, existing slot (drivers.rs:931-976):
  1. `context.property_feedback_slot` looks up `exec_function` by id, then the slot.
  2. `current_frame_is_strict`.
  3. `supports_fast_property_ic`, which is run **twice** (again in `run_store`, cache_ir.rs:401).
  4. `store_own_data_slot_atom` (object.rs:4054-4102): `read_payload` for the guard, `read_payload` for `mapped_argument_cell` (a host-data downcast plus a linear scan of mapped-argument entries), `with_payload` for the write, then `record_write`.
- **Template** existing slot (properties.rs:170-197): guard as for a load, value cell test, `str`, then:
  - a transition-shape barrier (`cbz w16` + `stp`/`ldp`, ic_probe.rs:740-757);
  - for pointer values, the value barrier, ≈15 on the fast path (values.rs:470-515).
- **Add transition** for a class instance (proto → `C.prototype` → `Object.prototype` → null): `PrototypeChainMissing` guards every link (cache_ir.rs:175-189; shape_transition.rs:90-99). Native lowering handles ≤2 links (shape_transition.rs:31-34). The Machine `PropertyStoreDispatch` then emits:
  1. ≈16 instructions per link (arm64.rs:2600-2640);
  2. capacity, extensible and `slab_len` checks;
  3. the store;
  4. `strh len` and `str shape` (arm64.rs:2640-2690);
  5. **two** barriers: the value, and the shape handle.

  ≈70-90 per field init (*est.*). The shape barrier is redundant: shapes are old and immortal (shape_body.rs:202-208).

### 3f. Runtime transitions, allocations, barriers, guards

- **Rust calls from generated code**: `STUB_JIT_LOAD_PROPERTY` and `STUB_JIT_STORE_PROPERTY` (miss or megamorphic in Template); the write-barrier slow path (values.rs:512-515). There are no other transitions on a hit.
- **Allocation**:
  - Only on the miss or slow paths: shape child and key interning (shape_runtime.rs:268-311), `reserve_slot_capacity` for an old slab (object.rs:2798-2841), the exotic sidecar, accessor cells.
  - Loading a non-length property off a primitive boxes it into a wrapper object (drivers.rs:174-200).
  - `alloc_stack_rooted_object_with_proto` collects all stack roots into a Vec per allocation (allocation_ops.rs:596-636), then calls `set_prototype` separately.

## 4. Invariants

- **Values and moving GC**: cells hold full addresses; the scavenger rewrites the low 32 bits; the cage is 4 GiB-aligned (tag.rs:23-43). Old space is non-moving mark-sweep; young is a semispace (otter-gc/src/lib.rs:1-8).
- **`values_ptr` is always current**: refreshed after every grow, spill or move, including inside `trace_slots_safe` after the memcpy (object.rs:2441-2459, 2747-2750). Debug builds verify it on every access.
- **Shapes**: pinned in old space, immortal, rooted by `transitions` and `handles_by_id` (shape_body.rs:202-208; shape_runtime.rs:190-208). The JIT bakes the u32 handle offset. Shape ids come from a process-global counter; snapshot restore bumps it (object.rs:115-130).
- **Slabs**: never resize, live in old space (slot_slab.rs:28-34). Element slabs likewise (elements.rs:25-26).
- **Barrier**: records the *parent object*, never the slot. The scavenger re-traces the whole parent, including the slab words through `values_ptr` (barrier.rs:24-33; object.rs:2751-2759).
- **IC probes and store misses are allocation-free**, so a failed probe can't relocate the receiver (cache_ir.rs:17-18; object.rs:4061-4066).
- **JIT inline probes have no safepoint**. The receiver is recomputed from the frame on every access; header pointers never survive a call (properties.rs:8-12). Only the cold call owns a safepoint (property_cfg.rs:6-10).
- **Prototype**: nothing is invalidated. Every hit re-reads `jit_proto` and re-guards the holder shape (ic_probe.rs:52-54). `set_prototype_value` only writes `jit_proto` and `chain_link_opaque` (object.rs:5219-5300).
- **Dictionary mode**: a null shape plus the per-object `dictionary_shape_id`. The `dictionary_layout` epoch advances on structural change, not on append (object.rs:795-808). A delete sets `DictionaryCompatible` and the object never takes an IC again (shape_cache.rs).
- **Megamorphic tables**: fixed boxed arrays, never resized, single-thread writes (property_cache.rs:35-37; property_cache/jit.rs:9-13).
- **Deopt**: the speculative load exit precedes all effects and the interpreter re-executes the `LoadProperty` exactly once (property_speculation.rs:20-21).

## 5. Duplication and multiple representations

1. **Shape identity, four forms.**
   - `ShapeId` u64: needs a dereference to read. Keys the side tables, both megamorphic tables and `GuardShapeId`.
   - `ShapeHandle` u32: what the JIT and the mono path compare.
   - `dictionary_shape_id`.
   - `dictionary_layout` epoch.

   `handles_by_id` exists only to translate between the first two for JIT snapshots (jit_compile.rs:873-878). Because shapes are pinned and immortal, the handle alone would work as the identity.
2. **Key identity, seven forms.**
   - `AtomId`
   - `ShapeBody.transition_key` (a GC string)
   - `ShapeRuntime.interned_keys: Box<str>`, hashed on every transition take (shape_runtime.rs:241-259)
   - the `NameInterner` `Mutex` map
   - `DictKeysBody` UTF-8 content hash (object.rs:1122-1128)
   - `ArrayExoticSlots` `IndexMap<String>`
   - `SymbolPropsBody`

   Lookups by spelling do an `eq_str` per shape link (shape_body.rs:311-329). `offset_cache` stores a full map per shape, which is O(n²) memory along a transition chain. It is filled on every runtime add-transition capture (property_dispatch.rs:95; shape_runtime.rs:334-352).
3. **Slot attributes, five forms**: shape `own_flags` / `own_is_accessor`; the `SlotMeta` table (materialized); `AtomOwnPropertyHit.is_data`; `AccessorCellBody`; and the `SlotData` / `PropertyDescriptor` / `PropertyLookup` interchange types. Uncached attribute reads walk the chain O(depth) (object.rs:2363-2372).
4. **Slab base, three computations**:
   - `emit_slab_base` branches on the slab handle and ignores `values_ptr` when inline (values.rs:329-357).
   - `emit_prototype_guard` uses `values_ptr` directly (ic_probe.rs:2029-2033).
   - The megamorphic probe uses inline-vs-slab with bounds checks (megamorphic_property.rs:118-137).

   `values_ptr` exists to make this uniform, but it isn't used uniformly.
5. **Prototype storage**: `jit_proto`, `ExoticSlots.proto_override` and the `ObjectPrototype` enum. Arrays use an implicit realm prototype plus a sidecar override. Map, Set, TypedArray, Promise and similar objects use a separate "expando" `JsObject` bag (property_dispatch.rs:1171-1460) plus the `get_walks_prototype_chain` family list (:1073-1086).
6. **IC representations**:
   - `CacheStub`, plus a separate monomorphic shortcut (`own_data_hit`);
   - the `JitCacheIrProgram` DTO;
   - Template emission;
   - Machine forms: speculated proof/slot, `PolymorphicLoad`, `StoreDispatch`, per-op `CacheIr*`, megamorphic, `constructor_effects`;
   - `PropertyLookupCache`;
   - `StoreTransitionCache` (two parallel arrays, property_cache.rs:227-230);
   - `MethodFeedbackDirectory` / `MethodCallIc` (interp/feedback.rs:35-48);
   - intrinsic programs, `constructor_field_transition_cache`, `object_literal_layouts`, `ObjectLayoutCache`.

   Resolvers: `resolve_atom_data_slot`, `lookup_own_atom`, `lookup_own(&str)`, `lookup(&str)` and `ordinary_get_value`.
7. **Empty objects, two birth states**: 56 call sites create `{}` as a dictionary object with a fresh unique id (`alloc_object_with_roots`, object.rs:3085-3091), and 17 create shaped ones. The unique-id objects get repaired later by `migrate_slow_to_fast`.
8. **Feedback-slot lookup, three ways**: load uses `function.property_feedback_at`; store uses `context.property_feedback_slot(function_id)`; the JIT runtime uses `named_property_site`, which re-decodes the instruction.
9. **Redundant emitted work**:
   - The ordinary-state guard runs twice per Template program.
   - The header is re-decompressed per Machine op, even though the `Value` already *is* the header address.
   - The shape-publication barrier is unnecessary for immortal old shapes.

## 6. Required changes

### (a) A much cheaper hot path

1. **Use the `Value` bits as the header pointer.** This removes `mov w12,w9` + cage `movz`/`movk` + `add` (3-4 instructions) at every decode (ic_probe.rs:231-246; arm64.rs:2549-2556). In Rust, stop re-deriving through the Acquire `cage_base()` load (compressed.rs:257).
2. **Fold the three state bytes into shape identity.** Mode, opaque and overridden (object.rs:760-810) would force a shape change (or null shape) instead. A guard becomes one `ldr`+`cmp`+`b.ne`, and the duplicated 6-instruction blocks disappear. Mono load would drop to about 8 instructions (*est.*).
3. **Put the prototype into the shape and add a prototype validity cell or epoch, V8 Map-style.** Proto loads, method guards and add-transitions then become receiver-shape check + constant holder + validity check (hoistable). This deletes `LoadPrototype` hops, the `PrototypeChainMissing` chains, the proto-shape key in `StoreTransitionCache` and the `UNRESOLVABLE` proto pinning. It requires changing all of these together:
   - `TransitionKey` (shape_runtime.rs:54);
   - every allocation path, which currently calls `set_prototype` after allocating (allocation_ops.rs:631, 791);
   - `set_prototype_value` (object.rs:5219);
   - `proto_override` and dictionary objects;
   - both megamorphic tables;
   - `CacheOp`, `JitCacheIrOp`, Template, Machine, and the method guards.
4. **Use the shape handle as the only identity.** The megamorphic probe then loses a dependent load, and `GuardShapeId` loses its dereference.
5. **Larger in-object slot capacity with slack tracking, and young slabs.** Currently every object with more than 3 fields allocates an **old-space** slab (heap.rs:2628; object.rs:3135), including short-lived AST nodes. That adds to old-space allocation, sweep work and remembered-set pressure (*speculation* on the size of the impact for `ast_ctor`/`ts`).
6. **Drop the per-hit atomic counters** (jit_feedback.rs:518) and remove the second `supports_fast_property_ic` call and the `mapped_argument_cell` check from the store hit (object.rs:4093).
7. **Template**: probe the shared megamorphic table inline, and allow re-patching when a site's IC fills after compile. The docs describe a "self-patching cell" (properties.rs:4-8; ic_probe.rs:41-50, which names a non-existent `WHISKER_IC_WAY_BYTES`), but the code emits immutable snapshots.
8. **Machine**:
   - Lower a whole CacheIR program to one fused proof node with a deopt exit, and extend speculation to prototype and polymorphic loads.
   - Replace the O(d²) `migrate_slow_to_fast` call on every interpreter miss with a one-time migration.
   - Give shapes a flattened descriptor array so misses become O(1).

### (b) A portable AOT artifact

These values are baked into code with **no relocation**:
- **Shape tokens**: `emit_check_shape_identity` loads them as plain immediates (ic_probe.rs:273-290), as do polymorphic and store-dispatch cases (arm64.rs:2398, 2595+). Only `PrototypeShape` goes through `GuardedHeapReference` (ic_probe.rs:2003-2014).
- **Atom ids**, which depend on interner order (megamorphic_property.rs:66-83).
- **`dictionary_layout` epochs** (ic_probe.rs:1949-1962).

Values that are already relocated: cage base, the two tables, `PropertySourceCell` and realm prototypes (relocation.rs:108-160).

What a portable artifact would need:
- a content-addressed shape identity (root + (atom spelling, flags, accessor, proto-identity) path), or a relocation per shape token resolved at load time;
- atom relocations by spelling;
- replacing the process-global `NEXT_SHAPE_ID` counter (object.rs:123);
- **or** pairing the artifact with a heap image that restores shapes at identical cage offsets (snapshot support exists: `register_restored_shape`, shape_runtime.rs:176-179), with atom tables fixed by that image.

Field offsets are already fixed by build-time layout asserts (object.rs:2088-2109).

## 7. Open questions

1. Whether Template code is ever recompiled after a property site's IC fills. The feedback epoch is documented as ignored by the baseline tier (executable.rs:~610), which would pin empty sites to the runtime stub. I couldn't confirm either way.
2. The actual `size_of` of `CacheStub`, `CodeBlockPropertyFeedback`, `ArrayBody` and `ShapeBody`, and total per-site feedback memory in `ts`. These affect the RSS gap and need measurement.
3. How much of the scavenger `process_slot` and old-space sweep time comes from old-space slabs of short-lived objects and their remembered-set entries (slot_slab.rs:164-167). The profile can't separate this.
4. Realm prototypes can be young at allocation time (allocation_ops.rs:780-786), yet `GuardedHeapReference::Prototype` bakes their offsets. I didn't verify the tenuring guarantee that makes this safe.
5. How often dictionary-born objects (the 56 `alloc_object_with_roots` call sites) reach hot property sites and fill PICs with unique ids before migration.
6. The share of prototype-chain property loads (as opposed to method calls) in `earley-boyer`, `ts` and `ast_ctor`, which decides how much the prototype-in-shape change is worth.