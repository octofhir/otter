# Task 10 — String Methods Slice (M4)

## Goal

Add the first practical family of `String.prototype` methods on top of the
string core from task `09`. Method dispatch on a primitive string receiver
must happen **without** allocating a wrapper object — this is the first
slice that exercises the primitive-receiver lookup contract from the
foundation plan.

## Scope

### JS surface covered

Implement (and only implement):

- `s.charCodeAt(index)`
- `s.charAt(index)`
- `s.slice(start [, end])`
- `s.substring(start [, end])`
- `s.indexOf(searchString [, position])`
- `s.startsWith(searchString [, position])`
- `s.endsWith(searchString [, endPosition])`
- string indexing `s[i]` (returns a single-code-unit string, or
  `undefined` for out-of-range, per spec)
- code-unit comparison via `<`, `<=`, `>`, `>=` for two strings

Method receivers are **strings only** in this slice. Calling a string
method on a non-string still produces a clear `TypeError`-equivalent
diagnostic. The full `ToObject`/wrapper rules arrive in a later slice
(M8/M9 in the foundation plan).

### Primitive-receiver dispatch contract

- A `JsString` value used as a method receiver routes through a single
  `string_intrinsic_dispatch` table keyed by interned method name.
- The dispatcher takes the string by reference, the argument list, and a
  small `MethodCallContext` carrying the runtime, the call frame slot
  for the result, and span information.
- **No** `JsObject` wrapper allocation occurs anywhere on this path. A
  fixture (and a Rust unit test) asserts that `"abc".length` and
  `"abc".charCodeAt(0)` perform zero object allocations.
- The dispatch table is built once at runtime construction.

### Bytecode

Add:

- `GetElement <dst> <obj> <idx>` — for a string receiver and an integer
  index, returns the indexed code-unit substring (or `undefined`); for
  other receivers, falls through to a `TypeMismatch` runtime error
  (objects/arrays land in their own slice).
- `CallMethod <dst> <recv> <name_const> <argc>` — the dispatcher above.
  The bytecode operand is a pointer into the constant table holding the
  method name (interned WTF-16). For the foundation subset, `name_const`
  must resolve to one of the string intrinsics; unknown names produce a
  diagnostic at execution time.
- `LessThan / LessEqual / GreaterThan / GreaterEqual` for two strings;
  for not-yet-supported pairs, defer to `TypeMismatch`.

Each new opcode entry follows the spec-mandated metadata fields per
task `06`.

### Compiler integration

Lower:

- string indexing → `GetElement`
- `s.method(args...)` → `CallMethod` for known string intrinsics where
  the compiler can prove (or speculatively assume — annotated for a
  future IC slice) the receiver is a string. In the foundation subset,
  the compiler proves it from local types only:
  - string-literal receiver
  - chained string-literal results (`"a".slice(0)`)
  - locals provably string-typed by the small dataflow tracker added
    here. Where proof fails, fall back to a generic call lowering that
    will be wired up in M8 (it is acceptable to raise a "feature not in
    this slice" diagnostic here).

### Lazy flatten policy

- `slice` and `substring` produce `Sliced` views without flattening when
  the parent is `Flat16`.
- `slice` over a `Cons` flattens once and then takes a view.
- `indexOf`, `startsWith`, `endsWith` operate directly on flat or
  cons-walked code unit streams. `indexOf` uses a bounded scan with a
  back-edge-equivalent interrupt checkpoint every 4096 code units (the
  foundation-plan native-loop polling rule).
- `charCodeAt` / `charAt` over a `Cons` walks the rope without flatten
  using an iterative descend; `charAt` constructs a one-code-unit
  `Flat16` (or returns the empty string for out-of-range).

### Tests

Engine fixtures under `tests/engine/strings/methods/`:

- `char-code-at-basic.ts`
- `char-at-basic.ts`
- `slice-positive-and-negative.ts`
- `substring-args-swap.ts`
- `index-of-found-and-missing.ts`
- `starts-ends-with.ts`
- `compare-lt-gt.ts`
- `index-on-rope.ts` — concat 1000 chars, then `s[500]` returns the
  expected code unit
- `slice-on-rope-no-flatten.ts` — `slice` on a flat parent returns a
  `Sliced` view (assertion via a debug API exposed only behind a
  `cfg(test)` feature)
- `surrogate-preserved-by-slice.ts`
- `not-string-receiver-throws.ts` — `(undefined).slice(0)` throws

Rust unit tests:

- `string_methods_zero_alloc_for_length_and_char_code_at`
- `index_of_polls_interrupt_after_n_units`

Benchmarks (extend `otter-vm/benches/strings.rs`):

- `index_basic` — index 1000 times into a flat string
- `index_of_short_in_long` — find a short needle in a 100 KiB haystack
- `slice_view_creation` — repeated slicing without flatten

## Out of scope

- `String.prototype.replace`, `match`, `search`, `split`, `repeat`,
  `padStart`, `padEnd`, `trim*`, `at`, `codePointAt`, `normalize`,
  `localeCompare`, `toLowerCase`, `toUpperCase`. Each is its own slice.
- Non-string receivers for any of the methods above.
- Tagged templates, regex receivers.
- ICU.

## Files / directories you may touch

- Edit / create under `crates-next/otter-vm/`,
  `crates-next/otter-compiler/`,
  `crates-next/otter-bytecode/`
- Create fixtures under `tests/engine/strings/methods/`
- Extend `crates-next/otter-vm/benches/strings.rs`

## Acceptance criteria

- All fixtures listed above pass under `otter test --suite engine
  --filter strings/methods/`.
- `string_methods_zero_alloc_for_length_and_char_code_at` passes.
- `index-on-rope.ts` confirms `O(log d)` (where `d` is rope depth)
  index access; the `index_basic` benchmark records the wall time.
- `index_of_short_in_long` benchmarks runs without timing out under a
  reasonable per-bench limit (≤ 5 s).
- Surrogate round-trip is preserved through every method.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passes.
- No new wrapper-object allocation on any string method path. Verified
  by the zero-alloc unit test and by `rg`-grepping for `JsObject::new`
  on the new string method module — there should be **no hits**.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine \
    --filter strings/methods/
cargo bench -p otter-vm --bench strings -- --quick
rg -n 'JsObject::new' crates-next/otter-vm/src/string \
    && exit 1 || true
```

## Risks

- **Wrapper allocation regression.** Easy to introduce by reusing a
  generic `obj.get_property` helper. The zero-alloc test is the gate.
- **Recursive rope walk.** `charCodeAt` on deep cons trees must not
  recurse. Use an explicit stack with `MAX_ROPE_DEPTH` ceiling.
- **`indexOf` hangs.** Long haystacks must poll the interrupt
  checkpoint. Add a test that interrupts a 100 MiB scan.
- **Argument coercion creep.** `start`, `end`, `position` arguments need
  the small-integer / NaN / undefined coercions from the foundation
  subset; full `ToInteger` arrives in task `11`. Until then, accept
  integer literal and numeric-typed locals only, and produce a clear
  diagnostic for other shapes.

## Next task

Proceed to [`11-number-core-slice.md`](./11-number-core-slice.md).

## Status

- not started
- last update: —
- artifacts: string-method intrinsics dispatch, opcodes, fixtures,
  benchmarks
