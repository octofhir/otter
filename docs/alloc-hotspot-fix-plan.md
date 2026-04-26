# Allocation hot-spot fix plan — interpreter throughput without JIT

> Discovered while measuring holdout #2 effect on 2026-04-26.
> Baseline run (`s.length + s.charCodeAt(i)` × 200k) = 992 ms.
> Holdout #2 only shaved ~10 % because the dominant cost is **wrapper allocation per property access**, not the WTF-16 round-trip we just removed.
> 9.3 s for 2M property reads = ~4.6 µs / read = textbook per-call alloc on the hot path.

The interpreter is allocation-bound on every primitive-receiver property access. V8/JSC don't allocate boxing wrappers for `s.length`, `(5).toFixed(2)`, `true.toString()` — they walk the prototype chain directly with the primitive as receiver. We must do the same.

---

## P0 — Primitive-receiver wrapper bypass

**Problem.** Every `LdaNamedProperty` / `LdaKeyedProperty` whose receiver is a primitive calls
[`coercion.rs:property_base_object_handle`](../crates/otter-vm/src/interpreter/runtime_state/coercion.rs#L327)
which allocates a fresh `String` / `Number` / `Boolean` / `Symbol` wrapper object. For tight loops over primitive methods this is 1 allocation per opcode.

**Fix.** New runtime helper `primitive_property_lookup(receiver, prop) -> Option<PropertyValue>`:
1. `TAG_PTR_STRING` receiver:
   - `length` → return code-unit count from the GC string directly.
   - canonical numeric index < length → read code-unit-at, allocate the 1-char return string (this allocation is unavoidable per ES spec).
   - else → walk `String.prototype` via `property_lookup` directly — no wrapper.
2. Number → walk `Number.prototype` directly.
3. Boolean → walk `Boolean.prototype` directly.
4. BigInt → walk `BigInt.prototype` directly.
5. Symbol → walk `Symbol.prototype` directly.

In `LdaNamedProperty` / `LdaKeyedProperty`:
```rust
if !target.as_object_handle().is_some() {
    if let Some(value) = primitive_property_lookup(target, prop)? {
        activation.set_accumulator(value);
        continue;
    }
}
let handle = property_base_object_handle(target)?;  // existing slow path
```

**Receiver semantics.** §10.4.6 [[Get]] passes the original primitive as `Receiver`. Our existing comment at [dispatch.rs:1073-1075](../crates/otter-vm/src/interpreter/dispatch.rs#L1073) confirms we already preserve it. The wrapper allocation is purely for the prototype walk and can be eliminated.

**Side effect on `ordinary_get`.** It currently takes `ObjectHandle` for the lookup target. We thread a `RegisterValue`-flavoured shadow that picks the right prototype.

**Affected sites kept on the slow path** (genuine wrapper need):
- `Object.assign(target, "abc")` ([property_copy.rs](../crates/otter-vm/src/property_copy.rs)) — needs an own-property iterator on the boxed string.
- `iterator_open` ([runtime_state/mod.rs:1012](../crates/otter-vm/src/interpreter/runtime_state/mod.rs#L1012)) — `[...string]` — leave for now, low frequency.
- `Object.prototype.toString.call(primitive)` — explicit boxing.
- `with(primitive) { ... }` — same.

**Estimated effect:** 2M iters of `s.length + s.charCodeAt(i)` from 9.3 s to <500 ms (≥20× speedup). Real workloads (JSON parsing, hash chains, tokenizers) hit primitive method calls constantly.

**Stage 1:** TAG_PTR_STRING only (covers ~80 % of primitive method calls in real code).
**Stage 2:** Number / Boolean / BigInt / Symbol.
**Stage 3:** Audit + retire the `property_base_object_handle` legacy bridge for the `HeapValueKind::String` case (legacy primitive, post Strategy B sweep).

**Quality gates:** lib tests stay 1196 green, clippy clean, micro-bench delta captured here.

---

## P1 — Catalog of remaining hot-spot allocations

After P0 lands, the next biggest allocation hot-spots are:

### P1.1 — Index-key allocation in `LdaKeyedProperty`

Every `arr[i]` where `i` is i32 currently goes through
[`key_to_property_name`](../crates/otter-vm/src/interpreter/dispatch.rs)
which allocates a string for the index. `JsObject::get_index` already takes a usize directly. Fast-path: when `key.as_i32().is_some_and(|x| x >= 0)`, skip the string alloc entirely.

**Effect:** every `for (let i = 0; i < arr.length; i++) arr[i]` saves one alloc per iter.

### P1.2 — `(n).toString()` doesn't cache for small ints

`Number.prototype.toString` allocates a fresh string every call. C2 already cached small-int → static `&'static str` for JSON serialization (`small_int_str(n)` for 0..999); extend it to the JS-visible `toString` path so `(5).toString()` doesn't allocate.

### P1.3 — Iterator-result object alloc per `next()`

Every iterator `.next()` builds a fresh `{ value, done }` object. This is hot in `for...of`, `Array.from`, spread. Two options:
- (a) Pool a pre-shaped `IterResult` object on `RuntimeState`, mutate-and-return; cheap if guarded by GC barriers.
- (b) NaN-box the `done` bit + compact the result so the iterator-consumer reads `value` and `done` from registers without going through an object at all (V8's "optimized iteration").

Option (b) requires bytecode changes; option (a) is single-session.

### P1.4 — Argument vector per call

Already partially fixed (C6 buffer pool). Audit for remaining `Vec::with_capacity` per-call sites in spread / rest / construct.

### P1.5 — Match `Cons`/`Sliced`/`Thin` per call to `string_value()` clone

Several intrinsics `js.clone()` then `js.ensure_two_byte()` — each is an alloc. Worth a borrow-aware redesign once C2 lazy-string is fully GC-managed (depends on string holdout #1).

### P1.6 — TypeError message format on every throw

Throws like `runtime.alloc_type_error(format!("foo: {bar:?}").into())` allocate a String unconditionally even on the success path's panic-free typecheck. Thread `Cow<'static, str>` and only `format!` on the throw path.

### P1.7 — Closure name-property write per closure alloc

Every `alloc_closure` followed by `set_function_name` allocates a string for the `.name` property. For anonymous closures we either store a sentinel or the empty TAG_PTR_STRING (already interned).

---

## P2 — Architectural follow-ups (post P0/P1)

### P2.1 — Inline cache on `primitive_property_lookup`

Once P0 ships, the prototype-walk per call is still slower than V8's IC because we re-walk `String.prototype` every call. Cache `(receiver_kind, property) → slot_offset` per PC (extends the existing `PropertyFeedback` lattice).

### P2.2 — Polymorphic IC over receiver kind

The IC currently keys on `(shape_id, slot_offset)`. For primitive receivers we'd need a synthetic shape per primitive kind (V8 calls these "hidden classes" too). Modest scope — single shape per kind.

### P2.3 — Computed-goto dispatch

`#![feature(computed_goto)]` would buy ~30 % on dispatch; nightly-only, deferred.

### P2.4 — Microtask queue allocation pool

Promise.then / queueMicrotask wraps the callback in a fresh `Microtask` struct. Pool a fixed-size ring buffer; spill to heap only on overflow.

---

## Sequence

1. **P0 stage 1** (string primitive bypass) — measure delta. Target: 9.3 s → <500 ms.
2. **P0 stage 2** (number/bool/bigint/symbol) — measure each.
3. **P1.1** (index-key) — measure on array iteration.
4. **P1.2** (toString cache for small ints) — measure on numeric loops.
5. **P1.3a** (iterator-result pool) — measure on for...of.
6. **P1.4** through **P1.7** — opportunistic.
7. Reassess vs. Bun / Node single-thread interp throughput.

Each item lands as its own commit only after the bench delta is recorded here.

---

## Bench harness conventions

Save scripts under `/tmp/otter-bench/*.js`; not committed. Bench format:
```js
const ITERS = 200_000;
const t0 = Date.now();
for (let i = 0; i < ITERS; i++) { /* hot path */ }
const t1 = Date.now();
console.log(`<label>: ${t1 - t0} ms (${ITERS} iters)`);
```

Run release binary 3× per measurement; report median + stddev. Compare against the worktree at HEAD (pre-change) using
`CARGO_TARGET_DIR=/tmp/otter-baseline-target cargo build --release --manifest-path /tmp/otter-baseline/Cargo.toml -p otterjs`.
