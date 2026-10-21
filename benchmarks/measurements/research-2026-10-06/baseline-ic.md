# Baseline property ICs from runtime feedback

Sources read (2026-10-06, `main`): `v8/src/baseline/baseline-compiler.cc`
(VisitGetNamedProperty / VisitSetNamedProperty / VisitCallProperty),
`v8/src/ic/accessor-assembler.cc` (LoadIC, TryMonomorphicCase,
HandlePolymorphicCase, LoadIC_Noninlined, HandleLoadICHandlerCase,
HandleLoadICSmiHandlerLoadNamedCase, HandleProtoHandler,
CheckPrototypeValidityCell, TryProbeStubCache, Generate*Baseline),
`v8/src/ic/stub-cache.h`, `v8/src/ic/handler-configuration.h`,
`v8/src/ic/ic.cc` (IC::SetCache, LoadIC::UpdateCaches, LoadIC::ComputeHandler,
UpdatePolymorphicIC), `v8/src/objects/map.cc`
(GetOrCreatePrototypeChainValidityCell), `v8/src/objects/js-objects.cc`
(InvalidatePrototypeChainsInternal), `v8/src/execution/frame-constants.h`
(UnoptimizedFrameConstants / BaselineFrameConstants).
Cross-check: `WebKit/Source/JavaScriptCore/jit/JITPropertyAccess.cpp`
(emit_op_get_by_id), `jit/JITInlineCacheGenerator.cpp`
(generateGetByIdInlineAccessBaselineDataIC), `bytecode/InlineCacheHandler.h`,
`bytecode/InlineCacheCompiler.cpp` (getByIdLoadHandlerImpl,
emitDataICCheckStructure, emitDataICJumpNextHandler);
`gecko/js/src/jit/BaselineCacheIRCompiler.cpp` (AttachBaselineCacheIRStub).

## V8 (reference)

### Per-site data: the feedback vector
- One `FeedbackVector` per closure feedback cell; a named load/store slot is
  TWO tagged words: `[feedback, extra]`.
  - uninitialized: `feedback = UninitializedSymbol`.
  - monomorphic: `feedback = weak(map)`, `extra = handler`.
  - polymorphic: `feedback = WeakFixedArray[map0, handler0, map1, handler1…]`
    (`max_valid_polymorphic_map_count` = 4), `extra` unused for named.
  - megamorphic: `feedback = MegamorphicSymbol` → isolate stub cache.
- Baseline (Sparkplug) frames store the feedback vector at a fixed fp slot
  (`UnoptimizedFrameConstants::kFeedbackVectorFromFp`); IC builtins read it
  with `LoadFeedbackVectorFromBaseline()`; the bytecode operand carries the
  slot index.

### Generated code: one shared IC builtin per isolate
- `VisitGetNamedProperty` → `CallBuiltin<LoadICBaseline>(receiver, name, slot)`.
  No per-site code beyond the call. Same for `StoreICBaseline`,
  `KeyedLoadICBaseline`; `CallProperty` is `GetNamedProperty` + `Call` with
  its own call feedback slot.
- `LoadIC` (accessor-assembler.cc:3500):
  1. `TryMonomorphicCase`: `feedback == weak(receiver_map)` → handler = `extra`.
  2. `HandlePolymorphicCase`: backwards loop over (map, handler) pairs.
  3. `LoadIC_Noninlined`: megamorphic → `TryProbeStubCache(name, map)`.
  4. miss → `Runtime::kLoadIC_Miss(receiver, name, slot, vector)`.
- `HandleLoadICHandlerCase`: Smi handler → decode kind; `LoadHandler`
  (DataHandler) → `HandleProtoHandler`; weak `AccessorPair` → call getter;
  Code → tail call.

### Handlers (handler-configuration.h)
- Smi handler: `KindBits(4)`, then per kind. `kField`:
  `StorageOffsetInWords | IsInobject | IsDouble | DescriptorIndex`.
  Kinds: Element, Normal (dictionary + entry hint), Global, Field,
  ConstantFromPrototype, AccessorFromPrototype, NativeDataProperty,
  ApiGetter, Interceptor, Slow, Proxy, NonExistent, ModuleExport, Generic.
- `LoadHandler` (DataHandler heap object): `smi_handler` (kind for the
  holder), `validity_cell` (or `kNoValidityCellSentinel` Smi), `data1`
  (weak holder | constant | getter), optional `data2/3` (access-check
  context, lookup-on-receiver dictionary negative).
- `HandleProtoHandler`: check validity cell (`cell.value !=
  kPrototypeChainInvalid`), optional receiver dictionary negative lookup,
  then `data1`: Smi → constant result; weak holder → run the smi handler on
  the holder; null → holder = receiver.
- `ComputeHandler` (ic.cc:1144): DATA own field → Smi `LoadField`; DATA on
  prototype → `LoadFromPrototype(map, holder, LoadField)` or
  `LoadConstantFromPrototype` (const field: value stored weakly in data1);
  dictionary holder → `LoadNormal`; ACCESSOR own → weak AccessorPair;
  ACCESSOR on prototype → `LoadAccessorFromPrototype` (getter in data1);
  not found → `LoadNonExistent(map)` (validity cell over full chain);
  JSFunction `.prototype` → `LoadIC_FunctionPrototype` builtin;
  string `.length` → `LoadIC_StringLength`.

### State machine (IC::SetCache)
UNINITIALIZED → MONOMORPHIC (`UpdateMonomorphicIC`); MONOMORPHIC miss:
`TryHealMonomorphicIC` (same handler, transitioned/deprecated map),
else → POLYMORPHIC (`UpdatePolymorphicIC`: drops cleared and deprecated maps,
replaces a map that is a transition of the new one, caps at 4); overflow →
`CopyICToMegamorphicCache` (all (map, handler) pairs into the stub cache) →
MEGAMORPHIC (each miss `stub_cache()->Set(name, map, handler)`).

### Megamorphic stub cache (stub-cache.h)
Isolate-wide, primary 4096 + secondary 1024 entries of `{name, handler, map}`.
Primary hash = `name.raw_hash + (map ^ map >> 12)`; secondary =
`(map + name) + ((map + name) >> 10)`. Insert moves the primary victim to the
secondary. No invalidation: handlers carry their own validity cells.
Cleared at mark-compact.

### Prototype validity cells
`Map::GetOrCreatePrototypeChainValidityCell(receiver_map)`: one `Cell` per
prototype map (`prototype_validity_cell`), reused while valid. Prototypes
register their users; any prototype map change runs
`InvalidatePrototypeChainsInternal` (walks `prototype_users` towards leaves,
setting `kPrototypeChainInvalid`). A handler checks one cell regardless of
chain depth.

## JSC cross-check (Baseline DataIC / handler IC)
- Per-site `PropertyInlineCache` (StructureStubInfo) lives in the CodeBlock;
  baseline loads its address (`loadPropertyInlineCache(index)`), never bakes
  structure ids.
- Inline fast path reads DATA, not constants
  (`generateGetByIdInlineAccessBaselineDataIC`): `load32 structureID` →
  `branch32 != [cache + inlineAccessBaseStructureID]` → `load32 [cache +
  byIdSelfOffset]` → `loadProperty`. Prototype variant also loads
  `[cache + inlineHolder]`. Repatching never touches code.
- Miss calls `cache.m_handler.callTarget`: a chain of `InlineCacheHandler`s
  {structureID, offset, holder, next, callTarget, jumpTarget}; each handler
  kind is ONE shared thunk (`getByIdLoadOwnPropertyHandler`,
  `getByIdLoadPrototypePropertyHandler`, `getByIdMissHandler`, getter, …)
  that checks `structureID == handler.structureID` else jumps to `next`.
  The chain ends in the slow path (`operationGetByIdOptimize`).
- Prototype loads are guarded by watchpoints that clear the handler.

## SpiderMonkey cross-check
`ICEntry` → linked `ICCacheIRStub`s ending in a fallback stub. Each stub =
shared compiled CacheIR code (`CompileBaselineCacheIRStub`, cached per
CacheIR in the JitZone) + per-stub data (shapes, offsets) at
`stubDataOffset`. Attaching a stub never recompiles baseline code.

## Convergent architecture (all three)
1. Per-site IC state is MUTABLE DATA owned by the code block, readable by
   generated code; baseline code never bakes shapes/offsets.
2. IC code is SHARED (V8 builtin per isolate; JSC thunk per handler kind;
   SM code per CacheIR shape).
3. A handler describes what to do once the receiver shape matched:
   own field, prototype field/constant (validity cell / watchpoint),
   nonexistent, accessor, dictionary, slow.
4. Mono → poly(≤4) → megamorphic (shared (name, shape) table) at runtime,
   driven by the miss runtime function; generated code is unchanged.

## Otter today (2026-10-05 map)
- Per-site feedback exists per `CodeBlock` (`FeedbackVector`,
  `CodeBlockPropertyFeedback` → `PropertyIcEntry<CacheStub>`, ≤4 then terminal
  megamorphic, no eviction of dead shapes), but it is Rust-only data.
- Template BAKES: `bake_property_cache_ir` snapshots `CacheStub` programs at
  compile time; arm64 inlines only `programs.len() == 1`; x86 inlines all.
  Later feedback never reaches running Template code.
- Miss → per-code-object shared subroutine probes the isolate
  `PropertyActionCache` (512×4, keyed (shape_id u64, atom)) — this already is
  V8's stub cache — then `STUB_JIT_LOAD_PROPERTY` which re-decodes the site
  from `(function_id, pc)` and probes the per-site PIC in Rust (77% of ts
  stub calls are such Rust PIC hits).
- Functions (closures, class constructors, natives) are not `ObjectBody`;
  every asm path rejects them by type tag → all `f.call`, statics, function
  expandos go to Rust uncached.

## Port: what Otter gets

### Feedback slot (V8 slot pair, native layout)
Per Load/Store/CallMethodValue site, a `#[repr(C)]` `PropertyIcSlot` in a
per-CodeBlock boxed slice (stable address; generated code holds the slot
address as an immediate — code and feedback are both per CodeBlock, the
Otter equivalent of V8's vector+slot):
- word 0 `state`: low 32 bits = mono receiver shape handle, or 0;
  tag values for poly / megamorphic.
- word 1 `handler` (mono).
- `poly`: pointer to ≤4 `(shape, handler)` entries (same handler format).
- Rust-side companion owns the `Arc<PrototypeValidity>` keepalives.
Shape handles in slots are WEAK (V8 weak maps): the full-GC `sweep_weak`
pass clears entries whose shapes died before cells are reused; this also
lets a dead shape free its PIC entry (V8 `UpdatePolymorphicIC` drops
cleared maps; Otter today keeps dead entries forever).

### Handler format (V8 LoadHandler subset for Otter's object model)
Word handler (no allocation): kind | inobject | slot index.
Proto/absent handlers: {kind, holder_root shape (holder = its prototype,
fixed by the shape-holds-prototype model), slot, validity cell address}.
Kinds: OwnField, PrototypeField, NonExistent, Slow (runtime); stores:
StoreField, StoreTransition(from→to, slab growth flag, validity).

### Code
- Template inline fast path = JSC DataIC: compare receiver shape with
  `slot.state`, decode `slot.handler` for OwnField/PrototypeField, load.
- On mismatch: shared IC routine (V8 LoadIC): mono, poly loop,
  megamorphic `PropertyActionCache` probe, handler dispatch; miss →
  `LoadIC_Miss` Rust runtime that computes the handler, updates the slot
  state (mono → poly → megamorphic with V8 healing/dropping rules) and
  completes the load.
- Interpreter uses the same slot and the same miss function (one model).
- Graph reads (shape, handler) pairs for speculation instead of CacheStub
  programs.

### Function receivers
V8 functions are JSObjects with maps, so `f.call` is ConstantFromPrototype
and statics are own fields. Otter function bodies need a shape word at the
ObjectBody shape offset to participate (closure body layout change); until
then the handler pipeline treats them as Slow. Tracked as a follow-up of
this subsystem (object-model work), not a heuristic in the IC.

## Otter implementation design (concrete)

### `PropertyIcSlot` (otter-vm, replaces `CodeBlockPropertyFeedback` +
`PropertyIcEntry<CacheStub>` + `PropertySourceCell`)
```
#[repr(C, align(8))]                       byte
struct PropertyIcSlot {
    inline_shape: AtomicU32,   //  0  entry0.shape iff entry0 is an own-field
    inline_field: AtomicU32,   //  4  load / own-field store; EMPTY otherwise
    state: AtomicU32,          //  8  count:8 | attempted | megamorphic
    atom: u32,                 // 12  property atom (stub-cache key)
    function_id: u32,          // 16  miss decodes the site (was PropertySourceCell)
    instruction_pc: u32,       // 20
    entries: [IcEntry; 4],     // 24  V8 (map, handler) pairs, inline array
}
#[repr(C)] struct IcEntry {    // 24 bytes
    shape: AtomicU32,          //  0  receiver shape handle (weak), EMPTY = u32::MAX
    field: AtomicU32,          //  4  FieldLocation raw (bank bit + word index)
    kind: AtomicU8,            //  8  handler kind
    aux: AtomicU32,            // 12  holder_root (proto kinds) / target shape (transition)
    validity: AtomicU64,       // 16  Arc<PrototypeValidity>::into_raw or 0
}
```
- EMPTY shape sentinel `u32::MAX` (never an 8-aligned cage offset), so the
  inline compare needs no zero test.
- Handler kinds: `OwnField`, `PrototypeField`, `NonExistent` (load);
  `StoreField`, `StoreTransition` (store). Anything else stays in the Rust
  miss (accessors, proxies, dictionary, non-ObjectBody receivers).
- Weak: shape/aux handles are pruned in `ExtraRootSource::sweep_weak` (full
  GC) before cells can be reused; no per-GC root tracing of IC slots.
  A dead entry is removed and the array compacted (V8 drops cleared maps).
- Validity cells: raw `Arc` owned by the entry; dropped on replace/clear.

### State machine (`LoadIC_Miss` / `StoreIC_Miss`, Rust)
uninit → mono → poly (≤4, `PROFILED_PROPERTY_PIC_CAPACITY`) → megamorphic.
On a miss with a cacheable answer: replace an entry with the same receiver
shape (healing: V8 `TryHealMonomorphicIC`), drop entries whose validity
died, else append; on overflow → megamorphic, copy every entry into the
isolate `PropertyActionCache` (V8 `CopyICToMegamorphicCache`), clear
entries. Megamorphic misses record into the stub cache only.

### Generated code
- Inline (Template, every named load/store site): cell + ObjectBody tag,
  `ldr w shape`, `ldp w,w [slot+0]`, `cmp`, bank select, `ldr`/`str`.
- Shared routine (V8 LoadIC/StoreIC, one per code object for now): entries
  loop with handler dispatch, megamorphic `PropertyActionCache` probe, Rust
  miss with `(ctx, receiver[, value], slot)`.
- Graph: unchanged consumer of the compile-time `JitCacheIrProgram` DTO,
  now derived from slot entries (V8 compilers read processed feedback).

## Landed (step 1, 2026-10-06)
- VM: `property_ic::PropertyIcSlot` replaces `CodeBlockPropertyFeedback` +
  `PropertyIcEntry<CacheStub>` + `CacheStub`; handlers OwnField /
  PrototypeField / NonExistent / StoreField / StoreTransition; V8 SetCache
  (heal same shape, drop dead proofs, ≤4, terminal megamorphic). Weak shape
  handles pruned in `sweep_weak`; per-GC IC root tracing removed (only the
  fixed 2048-entry shared table still traces transition targets).
- `PropertySourceCell` deleted in every tier; misses pass the slot address,
  whose bound `(function id, pc)` decodes the site. Relocation
  `propertyIcSlot(fid, bytePc)`.
- Template arm64 + x86: inline own-field DataIC + per-code-object
  LoadIC/StoreIC/method routines over the slot (+ shared table when
  megamorphic). No feedback is baked into Template code anymore; static
  intrinsic-prototype programs (primitive strings, closures, collections)
  stay inline before the IC.
- Graph unchanged as a consumer: `JitCacheIrProgram` snapshot derived from
  handlers at compile time.
- Numbers (instructions retired): ts 118.3G → 103.8G, crypto 8.66 → 8.49G,
  others flat. jit_property_transitions slab test now passes (stores stay in
  generated code).

## Next in this subsystem
- Receivers without a shape word (closures, class constructors, natives,
  arrays, primitive strings) still miss to Rust except static intrinsic
  programs. V8 gives every heap object a map; Otter needs a receiver class
  key for these bodies (a shape word at the ObjectBody shape offset for
  function bodies), then OwnField/PrototypeField handlers serve `f.call`,
  constructor statics and function expandos.
- Accessor handlers (V8 AccessorFromPrototype / own AccessorPair): getter
  call from the IC routine.
- One isolate-wide routine instead of one per code object (V8 builtins).
