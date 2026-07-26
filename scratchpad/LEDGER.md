# Engine velocity ledger

One row per slice of `scratchpad/PLAN_CACHEIR_SUBSTRATE.md`.

The column that decides whether the refactor is working is **LOC/%**:
hand-written lines added, divided by measured percent won. It must fall
slice over slice. If it stops falling, the substrate is not paying for
itself — revert the slice, do not extend it.

Metric: retired instructions from `just cost` (stable on this host; wall
time is a sanity check only). Kernels are a thermometer, never a target.

| Slice | What landed | Hand LOC | Won % | LOC/% | Note |
| --- | --- | ---: | ---: | ---: | --- |
| 0 | `just cost`, `just gate`, this ledger | 0 | — | — | tooling, nothing replaced |
| 1 | CacheIR lowered generically; one probe emitter; megamorphic re-probation | +366 / −322 | 87.7% | 4.2 | engine code net **−57 lines** |
| 2a | Collection method slot byte; optimizing tier bakes and emits allocating collection + array calls | +46 / −4 | 26.6% | 1.7 | `Map.set` 24.80ms → 6.94ms |
| 2b | Shared exotic-`length` emitter, both tiers | +101 / −37 | 7.1% | 14.2 | string `.length` 4.42ms → 1.24ms |

## Slice 1 result — 2026-07-26

Retired instructions on `property-polymorphic`, the kernel the change targets:

| Tier | Before | After | Factor |
| --- | ---: | ---: | ---: |
| template | 34 571 627 516 | 6 672 884 852 | **5.18x** |
| production-tiered | 33 862 438 798 | 4 247 663 895 | **7.97x** |

Runtime property stubs per invocation — the metric Slice 1 named as its target:

| Tier | Before | After |
| --- | ---: | ---: |
| template | 1 197 000 | 42 751 |
| production-tiered | 1 199 786 | 6 912 |

Against the bar (`just vs`, ms per `engineKernel()`):

| | Before | After |
| --- | ---: | ---: |
| otter | 60.66 | 8.12 |
| vs node | 25.39x | **3.34x** |
| vs bun | 42.07x | **5.46x** |

The polymorphic kernel went from the worst gap in the suite to the smallest.
Isolated, a pure prototype-load kernel went from 15 561 008 runtime property
stubs to **4** — one install per shape, everything after it inline.

No other kernel moved: retired instructions for `numeric-leaf` are
763M/866M before and after. A 2.83ms wall-clock reading for it during the
run was thermal noise, which is exactly why retired instructions is the
metric and wall time is only a sanity check.

Two defects were behind the 25x, both structural rather than local:

1. **The inline cell could not express a prototype hop.** It stored
   `shape → slot byte`, so `whisker_load_cell_fill` filtered stubs through an
   `own_data_hit()` recognizer and a prototype stub could never produce a
   fill. Every `o.bias` read left generated code for the runtime, forever.
   Fixed by lowering the CacheIR op program itself — the way now carries the
   guarded holder shape, and one shared emitter performs the guarded hop.
2. **Megamorphic was a death sentence.** A store that adds a property gives
   each receiver a new shape, so a site reading four objects across one
   `o.acc = v` sees eight shapes, exhausts the four-way PIC, and was then
   "never re-populated for the interpreter lifetime" — taking every tier's
   inline probe down with it, since generated code can only cache what the
   site still describes. Fixed by re-probation: a megamorphic site absorbs a
   miss budget, then tries again. Settled sites re-cache; genuinely
   megamorphic ones pay one re-probation per budget.

## The bar — otter vs node and bun, 2026-07-26, before Slice 1

`just vs`. Milliseconds per `engineKernel()`, startup excluded on both
sides, median of 20 after 8 warmups. Otter on `production-tiered`.
**This is the number that says whether the engine is good.** Internal tier
ratios only say whether a change helped.

| Kernel | otter | node | bun | otter/node | otter/bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| method-call-monomorphic | 4.5053 | 0.9463 | 0.4866 | 4.76x | 9.26x |
| numeric-leaf | 1.6847 | 0.0510 | 0.0313 | **33.03x** | 53.82x |
| branch-phi | 5.1176 | 0.6924 | 0.9391 | 7.39x | 5.45x |
| dense-array | 3.9922 | 0.8623 | 0.7264 | 4.63x | 5.50x |
| boxed-double-property | 4.1404 | 0.9387 | 0.9424 | 4.41x | 4.39x |
| property-polymorphic | 60.6562 | 2.3886 | 1.4419 | **25.39x** | 42.07x |

Reading the two outliers:

- **numeric-leaf 33x is partly an artifact.** V8 inlines `engineNumericLeaf`
  and constant-folds it — the arguments are the literals `2, 2`, so the body
  collapses to `-7` and the loop becomes an accumulate. Verified it is not
  eliminated: quadrupling the iteration count quadruples node's time
  (0.0547ms -> 0.2032ms). The missing capability is real (inlining plus
  constant folding in the optimizing tier), but no real program calls
  `f(2, 2)` a million times, so this gap is worth less than its size.
- **property-polymorphic 25x is the real one**, and it is what this plan
  already targets. Polymorphic property access is what actual JavaScript
  does.

Bytecode is not the cause. `just bcdiff property-polymorphic` puts otter's
loop body at 26 ops against V8 Ignition's 22 — comparable. The cost is
underneath: see the counter breakdown below.

## Baseline — 2026-07-26, before Slice 1

Retired instructions, `samples=20 warmup=8`, per `just cost`.

| Kernel | template | production-tiered | tiered/template |
| --- | ---: | ---: | ---: |
| method-call-monomorphic | 5 619 445 410 | 2 318 131 738 | 0.41 |
| numeric-leaf | 764 138 950 | 870 192 494 | **1.14** |
| branch-phi | 3 719 679 044 | 2 532 801 451 | 0.68 |
| dense-array | 4 538 915 344 | 2 606 864 642 | 0.57 |
| boxed-double-property | 6 343 049 636 | 2 141 165 412 | 0.34 |

Instructions per VM reduction (dispatch density):

| Kernel | template | production-tiered |
| --- | ---: | ---: |
| method-call-monomorphic | 197.3 | 82.7 |
| numeric-leaf | 231.4 | **308.9** |
| branch-phi | 131.3 | 90.4 |
| dense-array | 137.8 | 80.0 |
| boxed-double-property | 222.5 | 76.4 |

## Where property-polymorphic's 25x actually goes

Counters for one `engineKernel()` invocation on the template tier (400 000
loop iterations, three property accesses each):

| Counter | Per invocation | Per loop iteration |
| --- | ---: | ---: |
| `jit-runtime-property-stubs` | 1 197 000 | **3.0** |
| `jit-reentrant-stub-transitions` | 1 197 000 | 3.0 |
| `jit-runtime-stub-transitions` | 1 197 097 | 3.0 |
| `property-ic-load-hits` | 1 000 | 0.0025 |
| `property-ic-load-disables` | 3 (total) | — |

Every single property access in the JIT tiers leaves generated code and
re-enters the runtime. The interpreter's inline cache serves only the first
~1 000 iterations, before the loop tiers up; after that its hit counter
stops moving entirely. Three sites hit `load-disables`, meaning the
interpreter cache gives up on the polymorphic receiver set rather than
attaching a polymorphic stub chain.

That is the 25x, stated precisely: **the JIT tiers have no inline property
cache — they have a call into the runtime with a cache behind it.** It is
exactly what lowering `CacheOp` into generated code removes, so Slice 1 has
a measurable target: drive `jit-runtime-property-stubs` on this kernel
toward zero and re-run `just vs`.

## Slice 2 target, measured before any code

After Slice 1 every kernel reports a native-transition count of ~250 and a
property-stub count of 0. The suite had gone blind to exactly the axis Slice
2 attacks, so `native-boundary` was added: a loop of nothing but builtins —
string methods, `Math`, and a `Map` round-trip.

| | value |
| --- | ---: |
| otter | 82.22 ms |
| node | 3.83 ms |
| bun | 3.15 ms |
| **vs node** | **21.44x** |
| **vs bun** | **26.10x** |

Both axes light up, per invocation of 200 000 iterations:

| Axis | Per invocation | Per iteration |
| --- | ---: | ---: |
| `native` | 796 049 | 4.0 |
| `prop_stub` | 398 000 | 2.0 |

Two distinct causes, and they need different work:

1. **Property loads on non-ordinary receivers stay on the stub.** `word.length`
   on a string primitive and `table.get` on a `Map` are property loads whose
   receiver fails `supports_fast_property_ic`, so no cache program is ever
   built. That is CacheIR op-set coverage — the same shape of fix as the
   prototype hop.
2. **Every builtin call crosses the boundary untyped.** Four transitions per
   iteration through `NativeFastFn`'s `&[Value]` slice. That is the native
   descriptor work: types are known at macro expansion and thrown away.

Isolated per operation, `production-tiered`, 200 000 iterations each:

| Operation | prop_stub/iter | native/iter | wall |
| --- | ---: | ---: | ---: |
| `t.set(k, v)` + `t.get(k)` | 0.00 | 1.00 | **30.52 ms** |
| `Math.abs(i & 15)` | 0.00 | 0.00 | **12.67 ms** |
| `W[i & 7].length` | **1.00** | **1.00** | 4.35 ms |
| `W[i & 7].charCodeAt(i & 3)` | 0.00 | 0.00 | 2.27 ms |

Ranked, and two of the three are not what the aggregate suggested:

- **`Map.set` dominates at 30.5 ms** with one transition per iteration. The
  non-allocating read is already inline; the allocating write is not.
- **`Math.abs` costs 12.7 ms while crossing nothing.** Zero transitions and
  zero stubs, yet 63 ns per iteration against node's 19 ns for the whole
  six-operation loop. Its cost is in the guarded call sequence, not the
  boundary — so the descriptor work will not touch it and it needs its own
  measurement before anyone writes code for it.
- **String `.length` is the only property-stub source**: one stub *and* one
  transition per iteration, even though an inline primitive-string length
  path exists in the emitter. Its receiver is a primitive, so no cache
  program is built for it at all.

The kernel suite is now honest about both. `method-call-monomorphic` (4.67x
node / 9.02x bun) and `branch-phi` (7.39x node) remain, but their event axes
are zero and their bytecode is at parity with Ignition (18 ops against 20),
so what is left there is optimizing-tier code quality, not substrate work —
a different slice, and not one to start by hand-tuning.

## Slice 2, first target: `Map.set` never reaches its own fast path

Split from `Map.get`, both on `production-tiered`, 200 000 iterations over a
pre-filled 64-key map (so no `set` in the measured loop actually grows it):

| Operation | transitions/iter | wall |
| --- | ---: | ---: |
| `t.get(i & 63)` | 0.00 | 3.24 ms |
| `t.set(i & 63, 1)` | 1.00 | **24.72 ms** |

The transition is `jit-to-rust-call-transitions`, **not**
`jit-alloc-stub-transitions`, which is 0. So `set` is not paying for the
allocating collection entry — it never reaches it, and falls all the way back
to a generic method call into the runtime. Same disease as Slice 1: a fast
path exists and is unreachable.

Ruled out so far:

- Not the opcode gate. Both compile to `CALL_METHOD_VALUE`, which is what
  `bake_collection_alloc_methods` filters on.
- Not the stub registry. `COLLECTION_MAP_SET_ALLOC` has an entry, and the
  crate's own tests assert `has_entry()`, `entry_addr().is_some()` and
  `is_valid_for_safepoint(1)`.
- Not the safepoint sentinel. `NO_SAFEPOINT` is `u32::MAX`, so the first
  site's `safepoint_id` of 0 does not collide with it.
- Not the emitter conditions. `emit_alloc_method_guarded_call` only refuses
  on `cage_base == 0` or `value_arg_count != 3`; the cage base is live (the
  property probes use it) and the count is hardcoded to 3.
- Not `inline_leaf_template_plan`'s `collection_alloc_methods.clear()` — that
  view is for inlined leaf bodies, not the top-level loop.

**FIXED.** Two defects stacked, both found by instrumenting the bake path.

The first was the one the aggregate pointed at. There are two compile
snapshot builders in `crates/otter-vm/src/interp/jit_compile.rs`; the
optimizing one baked neither the allocating collection methods nor the array
methods, directly under a comment asserting it consumes the same baked inputs
as the template tier. The optimizing backend had no emission for them either.
Both now bake and both now emit, mirroring the leaf-method site that already
sat there.

That alone changed nothing, which exposed the second and real defect. In
`method_ops/jit_snapshot.rs` three builders compute the guarded method's slot
offset with `compressed_slot_byte`, and the two collection builders computed
it as `slot * size_of::<Value>()`. `Value` is eight bytes; the slab holds
four-byte `CompressedValue`. Every collection method's builtin-identity guard
was reading at twice the right offset, so it could never match and every call
fell through to the generic runtime path. Both now use `compressed_slot_byte`.

| | before | after |
| --- | ---: | ---: |
| `t.set(i & 63, 1)` wall | 24.80 ms | **6.94 ms** |
| transitions per iteration | 1.00 | **0.00** |
| native-boundary vs node | 21.44x | **15.63x** |
| native-boundary vs bun | 26.10x | **19.13x** |
| native-boundary retired | 38 299 538 507 | 29 597 938 475 |

**String `.length` fixed too.** `Op::LoadLength` exists in the bytecode and
has an inline emitter, but the compiler never emits it — a dead opcode with
live code behind it. Every `.length` goes through `LoadProperty`, and a
primitive string receiver fails `supports_fast_property_ic`, so no cache
program is ever built. The template tier had an inline arm for the *array*
exotic length only; the optimizing tier had no length arm at all.

Both now call one shared `emit_exotic_length_fast` covering the array and the
string case, next to the way walk and the prototype hop — the same
one-lowering-many-consumers shape as Slice 1.

| | before | after |
| --- | ---: | ---: |
| `W[i & 7].length` template | 4.42 ms | 1.83 ms |
| `W[i & 7].length` tiered | 4.42 ms | **1.24 ms** |
| property stubs per iteration | 1.00 | **0.00** |

Cumulative on `native-boundary`: 82.22 ms -> 56.08 ms, **21.44x -> 14.60x**
node and **26.10x -> 17.06x** bun.

Original investigation, kept because the ruled-out list is still the map of
this area:

**Root cause, found by instrumenting the bake path.** There are two compile
snapshot builders in `crates/otter-vm/src/interp/jit_compile.rs`. The
template builder bakes collection leaf methods, collection *alloc* methods,
array methods and primitive guards. The optimizing builder bakes only the
leaf methods and the primitive guards — directly under a comment asserting
that "the optimizing tier consumes the same baked compile inputs as the
template tier". It does not.

The kernel's loop runs in OSR-compiled optimizing code (`tmpl_entries` and
`opt_entries` are both 0, and `jit-optimized-osr-entries` is 1), so
`Map.set`, `Set.add`, `Map.delete` and every dense-array method are invisible
to the tier that actually executes the loop.

Baking them there was tried and reverted: it changes nothing, because the
optimizing backend has no emission for them either. `collection_alloc_methods`
and `array_methods` are read in exactly one place,
`crates/otter-jit/src/template/arm64/calls.rs`. Landing the bake alone would
publish metadata plus safepoint records that nothing consumes — a dead
forward contract, which is the thing this plan exists to delete.

So the work is emission, not feedback: the optimizing tier needs the
allocating collection and dense-array call sequences, with the alloc context
and precise safepoint publication its own frame model requires. That is
squarely Slice 2, and it is the first place where the two tiers genuinely
have different machinery rather than a shared description used twice.

Note for that work: a `set` on a key that already exists does not allocate at
all. Once the site reaches a guarded path, the honest question is whether it
should take the allocating entry at all, or a `MutatingLeafValue3` entry that
updates in place and misses to the allocating one only when the key is absent
— the shape `STUB_ARRAY_POP_LEAF` already uses. That signature family does
not exist yet: the mutating leaf carries two values and `set` needs three.

## Fixed to make the gate runnable

Both were sitting on `main` before this work and made `just gate`
impossible to pass, so they are gate infrastructure, not detours.

- `crates/otter-vm/src/binary/typed_array_prototype.rs` — `check_not_detached`
  was dead (clippy `-D warnings` failed the build). It is strictly weaker
  than `validate_typed_array`, which checks `is_out_of_bounds` and so covers
  the detached case; the spec-order revalidation work had already replaced
  every call site. Deleted.
- `crates/otter-bytecode/src/encoding.rs` — `coverage_matches_dispatcher_enum_size`
  asserted a hardcoded 172 against a table that now holds 178. The literal
  was the stale side, not the table. The property it claimed to guard is
  already enforced by the compiler: `schema_index`'s `match op` has no
  wildcard arm, so an `Op` variant missing from the schema list is a compile
  error, and density plus uniqueness have their own tests. Deleted rather
  than re-pinned to a new literal.

## Found, not fixed

Per plan rule: discoveries are recorded, not chased.

1. **The optimizing tier regresses `numeric-leaf`.** production-tiered costs
   1.14x the template tier's retired instructions, and its instructions per
   reduction are *higher* (308.9 vs 231.4) while it executes *fewer*
   reductions (2.82M vs 3.30M). So it emits denser-but-worse code on the
   one kernel the template tier had already closed. Every other kernel
   improves 0.34x–0.68x. Not touched in Slice 0.

2. **The kernels cannot exercise the IC axis.** `ic_miss` is 0–2 across all
   five: they are monomorphic and fully warm by design. That makes them a
   thermometer for dispatch and codegen, and blind to exactly the axis
   Slice 1 and Slice 2 attack. Before Slice 1's number can be believed,
   `just cost` needs at least one polymorphic, property-heavy workload
   (candidates already in tree: `benchmarks/run-v8-v7.sh`,
   `benchmarks/run-ejs.sh`). Add it as the first step of Slice 1, not as a
   separate detour.

3. **Native-boundary count is flat at ~6 800 per kernel** regardless of
   tier, and ~7 950 for dense-array. Constant across tiers means it is
   fixture/harness overhead, not kernel work — so this axis is also
   currently blind. It needs a real workload for the same reason as (2).
