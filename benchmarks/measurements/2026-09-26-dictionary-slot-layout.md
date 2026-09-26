# Global proofs retired by unrelated globals (2026-09-26)

Machine: Apple M1, release, tiered, median of 3. Before: `d0226a3f`.

## Cause

Generated global reads (`LoadGlobalOrThrow` of a `var` / function binding)
are proven by the global object's layout: it is in dictionary mode, so the
proof captured its dictionary structural id. That id is replaced by **every**
key-set change — adding any global, or redefining any global's descriptor.
The proof then misses on every execution into the runtime binding call
(which also decodes the inline activation chain), and nothing recompiles it.

Real programs add globals after code is compiled all the time. In this
engine the runtime itself does: the first `console.log`/`print` loads the
Node console shim, which appends `__otterRequireCache` / `Buffer` and
redefines a lazy global with `Object.defineProperty`. Profiling DeltaBlue
with a driver that repeats the suite (so it prints between repetitions) put
48% of isolate time in `jit_binding_value_stub`, all of it `Read(Global)`
misses whose captured layout was one behind the live one.

## Change

A proof about one existing key only needs that key to stay at its slot with
the same kind — the invariant V8 keeps with per-property `PropertyCell`s
and JSC with property watchpoints:

- `ObjectBody::dictionary_layout: u32` (in existing padding, body stays 88
  bytes): a dictionary slot-layout epoch. Appending a key to an object
  already in dictionary mode keeps it; entering dictionary mode, deleting a
  key, or clearing the key set advances it. Saturates at `u32::MAX`, which
  no proof captures.
- `SlotMeta::watched`: set by `watch_dictionary_slot` when a compiled proof
  reads the slot's value directly (global object reads/writes, the
  `%String.prototype%` data-property program). Redefining a watched slot's
  kind or attributes advances the epoch; redefining an unwatched slot does
  not. The structural id keeps its old meaning for every other cache.
- Global-object proofs, `GuardDictionaryLayout` and dictionary method
  holders guard the `u32` epoch in both tiers on both architectures.
  Method-holder proofs need no watch: their builtin-identity guard already
  rejects a redefined slot.

## Result (Octane single run, tiered, median of 3)

| suite | before | after | |
|---|---:|---:|---:|
| earley-boyer | 2055 | 2349 | 1.14x |
| raytrace | 1906 | 1973 | 1.04x |
| typescript | 743 | 758 | 1.02x |
| deltablue | 1582 | 1574 | |
| richards | 3078 | 3082 | |
| splay | 2587 | 2535 | |
| code-load | 3024 | 2979 | |
| pdfjs | 688 | 691 | |

The scored Octane run prints only after each suite, so DeltaBlue's score did
not carry the staleness; in the repeating profile driver its binding-stub
share dropped from 48% to 0%.

Tests: `jit_machine_inline_bindings` — appended globals keep generated reads
(< 100 reentrant transitions over 20,000 reads), and delete / accessor
redefinition / data redefinition of the proven global each miss into exact
semantics.
