# ObjectBody field usage map (HEAD f6acbbbc)

Input for the L2 staging (architecture report, section 12). `OBJ` =
`crates/otter-vm/src/object.rs`, `VM/` = `crates/otter-vm/src/`, `JIT/` =
`crates/otter-jit/src/`.

## Fixed body today (32 B after the 8-byte GC header)
`@0 shape u32, @4 slab u32, @8 jit_proto u32, @12 exotic u32, @16
dictionary_layout u32, @20 slab_len u16, @22 flags u8, @23 inline_capacity
u8, @24 dictionary_shape_id u64`, in-object slots from 32.

## Counts
| Field | Rust writers | Rust readers | JIT writers | JIT readers |
|---|---|---|---|---|
| shape | 19 + 4 inits + trace | ~31 in module, 24 `object::shape()`, 25 `object::shape_id()` | 10 | ~46 |
| slab | 2 + 4 inits + trace | 6 | 2 | 20 |
| jit_proto | 2 + 4 inits + trace | 1 accessor (OBJ:2524) | 2 | 29 |
| exotic | 1 (OBJ:2079) + 4 inits | ~27 + ~20 accessors | 2 | 4 |
| dictionary_layout | 1 fn (OBJ:2215) | 1 fn (OBJ:3701) | 2 | 6 |
| slab_len | 3 + init | ~12 + 2 external | 10 | 19 |
| flags | `set_flag` OBJ:917 only | 4 accessors | 2 | 13 + guard helpers |
| inline_capacity | inits only | 4 | 2 | 12 |
| dictionary_shape_id | 5 + 4 inits | 2 | 2 (zero) | 0 |

## Key sites
- Layout constants: OBJ:2133-2194 (asserts), frozen test OBJ:6955;
  `JitCompileSnapshot` fields VM/jit.rs:430-486, set in VM/executable.rs:326-352;
  flag constants VM/jit.rs:257-273.
- Receiver allocation writes 4 packed words: JIT/arm64/direct_call/receiver_allocation.rs:208-232,
  JIT/machine/numeric/x86_64/direct_call/receiver_allocation.rs:264-288.
- slab_len JIT writes (add transitions): template/arm64/ic_probe.rs:720,
  machine/numeric/arm64.rs:2776/2919, arm64 megamorphic 454, template/x86_64.rs:3045,
  machine/numeric/x86_64.rs:1447/1576, x86 megamorphic 498.
- slab_len JIT reads: bounds / exact-append, receiver-allocation bag proofs
  (arm64 127/143, x86 157/187), instanceof (arm64 103, x86 90).
- inline_capacity JIT reads: ic_probe 680; arm64 machine 2754/2882; x86 template 3000;
  x86 machine 1541/4954; megamorphic arm64 133/258/443, x86 132/269/487.
- flags JIT: `emit_ordinary_lookup_state_guard` ×14, `emit_shape_state_guard` ×8,
  `emit_load_header` ×9, `emit_check_shape` ×3 (ic_probe.rs:89-141, 272-304);
  x86 machine guards at machine/numeric/x86_64.rs:4927-4975; x86 template 3115-3190;
  EXTENSIBLE tests ic_probe 684, arm64 machine 2758/2886, arm64 megamorphic 428,
  x86 template 3004, x86 machine 1434/1545, x86 megamorphic 462.
- dictionary_layout guards: ic_probe.rs:1970, template/arm64/binding.rs:96,
  machine/numeric/arm64.rs:866, machine/numeric/x86_64.rs:1055/3635, template/x86_64.rs:2872.
- Dictionary entry points (`replace_dictionary_identity` OBJ:2208): 3089, 5148, 5480,
  5550, 5716, 5992, descriptor_core.rs:106; layout epoch also at OBJ:2425,
  shape_transition.rs:171/345; `watch_dictionary_slot` OBJ:3716.
- GC trace OBJ:2733-2801 (walks `[0..slab_len]`); VM/code_liveness.rs:123.

## Shape model
- `ShapeBody` (tag 0x22, VM/object/shape_body.rs:48-81): id, parent, transition key/atom,
  property_count, own_offset/flags/is_accessor. Allocated old (pinned), immortal
  (ShapeRuntime roots every handle, shape_runtime.rs:192-209).
- One isolate root; transitions keyed `(parent id, atom, flags, is_accessor)`.
- Shape ids: process-global atomic `NEXT_SHAPE_ID`; `UNASSIGNED = 0`.
- Guards compare the compressed ShapeBody handle; megamorphic tables compare
  the u64 id through `SHAPE_BODY_ID_OFFSET`.
- Null shape = dictionary mode (~25 Rust, 11 JIT checks).
- `ShapeEpoch` dependency: no production dependents.

## Prototype references outside bodies
StoreTransitionCache (receiver shape, proto shape, atom), PropertyLookupCache,
CacheIR `LoadPrototype`/`GuardPrototypeNull`, `MethodProtoChain`, `JitMethodGuard.proto_chain`,
`JitReceiverAllocationPlan.prototype_shapes`, constructor field transitions, instanceof
probes, pinned realm prototypes; `ClassConstructorBody.prototype`,
`ClosureRareBody.proto_override`, array/weak-ref prototype overrides,
`ExoticSlots.proto_override`.

## Risks for L2
Multiple `jit_proto` writers (OBJ:5372, 3216, receiver allocation); epoch bumped only
by the proxy-aware funnel; allocate-then-set-prototype everywhere; shape rebuilds
start from the global root; EXTENSIBLE / SLOT_ATTRS_OVERRIDDEN / CHAIN_LINK_OPAQUE
flip in place without shape change; null-shape dictionary sentinel; shape
immortality (prototype in shape would leak prototypes); property bags of
closures/natives/bound functions/class statics store owner extensibility in bag flags.
