# Inline closure capture storage

Measured on `d521b4cf`, macOS ARM64. Parity has not been reached.

## Evidence and design

After arguments elimination, Earley still creates 10,172,901 separate captured
spines (559,846,416 bytes) alongside 10,193,283 closures (1,141,647,696 bytes).
The post-arguments profile attributes 82/1739 self samples directly to spine
allocation; closure allocation adds 20. Shared allocator and collector cost is
not wholly attributable to those allocations.

[BEAM ErlFunThing](https://github.com/erlang/otp/blob/master/erts/emulator/beam/erl_fun.h)
stores its environment in a flexible array after the function header. Otter can
use the same ownership arrangement for compressed references to shared mutable
binding cells. This does not adopt Erlang binding semantics. V8/JSC grouped
lexical slots and the rejected Otter owner/index experiment are documented in
`2026-09-27-captured-environments.md`.

The change removes UpvalueSpineBody and its type registration. A closure owns
its four-byte capture references in trailing GC storage, initialized before the
allocator publishes outgoing edges. Pending tracing walks only fixed fields;
the source capture buffer is rooted separately until initialization. Heap-image
restore recomputes the derived call-header address from the relocated closure.
Both native backends consume the existing authoritative call-header offsets.
There is no alternative spine representation.

The production MakeClosure/MakeFunction sites already allocate closure bodies in
old space. The previously separate `alloc_closure_with_roots` entry used the
generational allocator but had no production call site. Both allocation entries
now use the same old-space policy and inline-tail initializer. Borrowed native
capture windows retain their stable-address guarantee. Reported RSS must still
be measured because the old-space size classes and collection schedule change.

The first candidate still copied capture cells through a heap Vec. The final
allocator borrows a mutable capture slice; interpreter and generated construction
use inline SmallVec buffers for up to 16 cells and parent indices, spilling for
larger environments. All test and VM callers use the same slice contract. This
removes unconditional host allocation without imposing a capture-count limit.

The exact baseline executable is preserved as `otter-before-closure-tail` under
`benchmarks/results/parity-2026-09-27/`, SHA-256
`f6069698a594b9b89e5a2218bcabdf863401b1dc692657597b8f2d152b628cbd`.
The same executable produced `arguments-count-final/`.

## Final fixed-work measurements

Final executable SHA-256:
`8885cc248dd3de844554a68be0a1bd03971436d22284ee44bc1bb5f07127c10b`.
All seven unchanged workloads exited successfully and produced identical stdout
in the sequential before/after pair. Raw `closure-tail-reference/` and
`closure-tail-final/` contain commands, workload hashes, host metadata and
`/usr/bin/time -l` output. Figures include process startup and compilation.

| Workload | Instructions before | Instructions after | Change | RSS before (bytes) | RSS after (bytes) | Change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| ts | 204,047,312,409 | 203,756,338,372 | -0.143% | 706,953,216 | 696,762,368 | -1.442% |
| zlib | 275,082,764,545 | 274,933,277,314 | -0.054% | 828,145,664 | 842,678,272 | +1.755% |
| crypto | 17,064,216,001 | 16,995,285,690 | -0.404% | 50,839,552 | 50,315,264 | -1.031% |
| fib | 3,846,950,209 | 3,845,882,430 | -0.028% | 59,490,304 | 59,392,000 | -0.165% |
| mega_method | 10,159,753,277 | 10,154,551,156 | -0.051% | 59,768,832 | 59,621,376 | -0.247% |
| ast_ctor | 32,654,624,876 | 32,600,339,445 | -0.166% | 130,334,720 | 130,023,424 | -0.239% |
| earley-boyer | 240,052,318,693 | 230,321,885,619 | -4.053% | 135,004,160 | 135,102,464 | +0.073% |

Small changes on the other six workloads are not established speedups. Crypto
uses the existing unseeded fixed-work source; do not interpret a fraction of a
percent as stable algorithmic improvement. RSS repetitions are reported below.

### RSS and instruction repetitions

Two additional pairs alternate candidate/reference and reference/candidate
order. Raw results, exact binary hashes and time logs are in
`closure-tail-repeat/`. Every repeated stdout matches its original baseline.

| Workload | Pair | Instructions before | Instructions after | Change | RSS before | RSS after | Change |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| zlib | 1 | 275,011,826,012 | 275,056,178,248 | +0.016% | 826,949,632 | 782,041,088 | -5.431% |
| zlib | 2 | 274,968,048,784 | 275,123,290,382 | +0.056% | 829,505,536 | 833,880,064 | +0.527% |
| earley-boyer | 1 | 239,987,263,268 | 230,448,859,791 | -3.975% | 134,823,936 | 134,496,256 | -0.243% |
| earley-boyer | 2 | 239,738,099,212 | 230,369,093,208 | -3.908% | 135,004,160 | 134,119,424 | -0.655% |

Retain the change for the repeatable Earley instruction reduction, fewer GC
objects and simpler ownership. Peak RSS remains unresolved: Earley changes
−0.66%..+0.07%, while zlib spans −5.43%..+1.75%. These observations do not support
a robust memory improvement or a repeatable memory regression. The large
remaining gap to V8/JSC remains the goal of subsequent work.

### Allocation census and compilation

Earley allocates 10,810,393,784 GC bytes, down from 10,974,003,256 (−1.49%).
It removes 10,172,901 separate capture-array objects; the final closure payloads
include their inline slots. Minor collections fall from 638 to 626 and full
collections from 257 to 244. Its 110,900,416 mutable binding-cell allocations
are unchanged. TS allocation bytes fall from 1,028,544,312 to 1,020,563,096
(−0.78%); minor/full collection counts remain 39/5.

Compiler event capture is a separate run. Earley records 160 successful
compiles, 94,830,919 ns total compile time and 1,660,600 code bytes, versus
159 / 86,229,589 ns / 1,656,084 bytes in the saved baseline event run. Zlib
records 57 attempts, 54 successes, 3,846,637,580 ns and 15,257,860 code bytes,
versus 3,880,175,167 ns and identical code bytes. The reports are untruncated;
these single timing observations do not establish a compiler speedup.

The tail-only pilot still used a temporary heap Vec. It measured Earley
239,822,864,450 → 238,760,672,921 instructions and RSS
135,020,544 → 125,861,888 bytes. The final allocator slice contract plus
SmallVec construction is a different executable. Its RSS does not reproduce
that pilot reduction; the pilot must not be presented as the final result.

### Remaining instruction gap

Ratios below use the original same-source V8/Node and JSC/Bun fixed-work
observations recorded in the parity investigation. They include host overhead;
they are not fresh cross-engine repetitions.

| Workload | Otter / V8 | Otter / JSC |
| --- | ---: | ---: |
| ts | 9.70× | 9.83× |
| zlib | 11.89× | 11.80× |
| crypto | 5.80× | 7.63× |
| fib | 4.04× | 7.13× |
| mega_method | 2.51× | 6.21× |
| ast_ctor | 9.89× | 15.82× |
| earley-boyer | 12.36× | 10.50× |

## Validation

- Required differential gate: **59/59**, after rebuilding the final CLI.
- Required JIT library gate: **283/283**.
- Captures and local arguments corpus: **96/96** executions, stress strides
  1..16, interpreter/Template/production, `OTTER_GC_VERIFY=1`, Node stdout match.
- Closure tests: **9/9**, including collection during pending allocation and
  a young captured child rewritten through two minor collections.
- Snapshot restore/shared mutable captures: **1/1**; upvalue cycle reclamation:
  **1/1**. Snapshot setup explicitly empties the nursery before capture.
- x86_64 JIT cross-compilation passes; x86 execution was not tested.
- `git diff --check` passes. No full Test262 or full runtime suite was run.

Final raw logs are `closure-tail-{difftest-gate.json,jit-gate.txt,
verified-tests.txt,snapshot-final-test.txt,cycle-test.txt,x86-check.txt}` and
`closure-tail-stress/results.json`. The intermediate differential run timed out
on one stress corpus under concurrent compilation; it is retained as
`closure-tail-initial-difftest-gate.json` and is not the final gate result.

## Next binding investigation

The compiler's JSON bytecode dump identifies eight of the ten own bindings in
`deriv_trees` as parameter values stored once in its entry prologue. Propagating
binding identities through MakeClosure capture maps finds no descendant stores
to those eight bindings. The function has no arguments object or direct eval.
This is a candidate for eliminating mutable cells, not a complete eligibility
proof or a dynamically weighted allocation count. Raw inputs/results:
`earley-capture-bytecode.json`, `earley-capture-candidates.json`,
`earley-deriv-capture-writes.json`. Standalone dump function IDs differ from
linked runtime IDs; compare names and source positions.

A representation worth evaluating is a four-byte tagged capture slot: compressed
GC references, small integers and primitive constants, with boxing for values
that do not fit. [V8 Smi](https://github.com/v8/v8/blob/main/src/objects/smi.h)
provides the concrete precedent for immediate small integers. This could avoid
the rejected experiment's doubled capture-reference width. It requires an
explicit compiler proof, moving-slot tracing, both generated backends and exact
value preservation for large numbers, negative zero, NaN and other primitives.
Mutable/eval/mapped-argument bindings must retain shared-cell identity. No such
representation is implemented by the inline-tail patch.

[V8 context specialization](https://github.com/v8/v8/blob/main/src/compiler/js-context-specialization.cc)
separates no-cell context loads from ContextCell loads and checks initialization
before replacing an immutable slot with a constant. An environment can escape
before its slot is initialized. Otter's eligibility proof must therefore cover
initialization order as well as the absence of later writes; parameter defaults,
TDZ captures and dynamic scope cannot be admitted from a write count alone.
