# Closure `prototype` storage: consumer map (2026-09-30, HEAD 60ddec15)

Read-only map for moving a closure's `prototype` out of its own-property bag
into the rare record (V8 `JSFunction::prototype_or_initial_map`). Paths are
under `crates/`.

## Why
`F.prototype = x` (every TS/ES5 class via `__extends`) materializes a
dictionary-mode bag and sets `CLOSURE_LOOKUP_OWN_PROPS`. That turns off the
closure CacheIR program (`interp/jit_intrinsic_properties.rs:67-86`, exact
`ORDINARY` guard at `jit.rs:906`), so `_super.call(...)` resolves `call` by
spelling each time, and it makes the construct and `instanceof` proofs walk
closure → rare → bag → shape → slot. A dictionary layout epoch is per object
(`object.rs:2141`, starts at 1), so no bag proof can distinguish two closures.

## Storage today
- `otter-vm/src/closure_construct.rs:48-55` `ClosureRareBody { own_props,
  prototype_shape, prototype_slot, learned_instance_fields, proto_override }`;
  offsets `:58-67`, trace `:83-91`, function ids `:94-98`;
  `prepare_closure_prototype_slot` `:115-153` (caller `call_ops.rs:526`).
- `closure.rs:207` `CLOSURE_LOOKUP_OWN_PROPS`; `:546-563` `own_props` /
  `set_own_props`; `:193-199` exact-ORDINARY guard.
- Bag plumbing: `function_ops.rs:1340` `callable_bag_read`, `:1373`,
  `:1388` `ensure_closure_rare`, `:1425-1467` `function_user_bag`;
  `property_dispatch.rs:222-268`.
- `execution_context.rs:487` `function_has_prototype_property`.
- Bare `Value::function` templates keep their bag in `function_user_props`
  (`lib.rs:1415`; traced `trace_roots.rs:151`, `runtime_state.rs:161`,
  `root_census.rs:269`).

## Readers and writers
- Materialization: `function_ops.rs:2619-2768`
  `function_property_get_with_receiver` (bag shadow `:2646`, kind gate
  `:2664`, bag `:2679-2680`, object `:2683-2728`, dictionary define `:2743`,
  `constructor` `:2751-2766`, generators `:2770`); wrapper `:2339`; callers
  `object_internal_ops/get.rs:268-285`, `property_dispatch/properties.rs:241-255`,
  `method_ops/mod.rs:1904`, `object_internal_ops/descriptors.rs:355-375`,
  `object_internal_ops/define.rs:414-426`, `call_ops.rs:3081`,
  `object_internal_ops.rs:688`, `call_ops.rs:1670`, `:3786`.
- Set/define/delete/keys/has: `properties.rs:852-877`, `:970-973`;
  `drivers.rs:1125-1156`, `:1236-1250`, `:265-300`, `:351`, `:575-585`;
  `function_ops.rs:1502-1523` (has own), `:1552-1590` (own keys), `:1672-1711`
  (own descriptor), `:1713-1868` (define), `:1870-1913` (delete),
  `:2073-2150` (defineProperty fast path); `set_delete.rs:125-130`,
  `:1074-1140`; `property_dispatch.rs:330-333`, `:558-562`; `keys.rs:563-575`;
  `object_internal_ops.rs:1026-1049`; `get.rs:1462-1476`; freeze/seal
  `define.rs:810-918` (generic).
- Construct: `call_ops.rs:486-531` (bag data read + proof), `:738-759`,
  `:3068-3122` `construct_prototype_for_callee` (callers `:2432`, `:2950`,
  `:4306`, `:755`; `otter-jit/src/entry/runtime_ops/reentry.rs:1266,1311`);
  layout `executable.rs:371-381`, `:1508-1518`, `jit.rs:305-318`,
  `:2542-2547`; generated `otter-jit/src/arm64/direct_call/receiver_allocation.rs:124-165`,
  `otter-jit/src/machine/numeric/x86_64/direct_call/receiver_allocation.rs:147-205`.
- instanceof: `function_ops.rs:954-998`, `object_internal_ops.rs:637-727`;
  `machine/numeric/arm64/instanceof.rs:56-110`,
  `machine/numeric/x86_64/instanceof.rs:38-95` (emitted `arm64.rs:2052`,
  `x86_64.rs:784`); the bag's symbol-props check proves no own `@@hasInstance`.
- Classes already hold `prototype` inline and non-writable
  (`class_constructor.rs:45,72,150`). Native and bound functions unaffected.
- GC: trace + function-id liveness (`code_liveness.rs:135`), barriers
  (`closure.rs:529-535`, `:554-563`). Snapshots copy the heap image; no
  serializer change.

## Existing spec bugs found
- `'prototype' in F` is `false` before materialization.
- After `F.x = 1`, `Reflect.ownKeys(F)` is `length,name,x,prototype`
  (spec: `length,name,prototype,x`).
- `Object.defineProperty(G, 'prototype', {value: {}})` leaves
  `writable: false` (must stay `true`).

## Invariants for the new slot
1. `{writable: true, enumerable: false, configurable: false}` for ordinary
   functions and generators; writable may go true → false only; the value
   changes only while writable; never an accessor; delete fails.
2. The property exists (has/in/GOPD/set/define) before the object is
   allocated; the default object is allocated once, with
   `constructor === F` (not for generators).
3. Key order `length, name, prototype`, then user keys; never duplicated.
4. Generated code reads the slot without allocation: a materialized-object
   check replaces the bag shape/slot proof.
5. Trace, write-barrier and function-id visit the new field; fixed offsets.
6. Bare `Value::function` templates keep working.
7. The `@@hasInstance` proof keeps reading the bag's symbol properties.
