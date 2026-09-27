# Indexed-access pipeline replacement

Status: all seven fixed-work programs and both required gates pass; parity is not reached.
Baseline: signed `bd9285bf2836cdf4f26231d16bef36a8b25b1fa3` (`G`).
The baseline executable is preserved as
`benchmarks/results/parity-2026-09-27/otter-before-element`.

## Measured reason

Zlib still retires 399,851,223,485 instructions versus Node/V8 23,130,083,733
and Bun/JSC 23,296,772,118. Peak RSS is 902,086,656 bytes versus 83,312,640
and 186,138,624. The uninstrumented baseline profile attributes 70.9% of
isolate self samples to generated code and 20.3% inclusive to optimizing
compilation.

An artifact profile starting ten seconds after launch gives 3,143 generated
self samples: 1,151 view probes (36.6%), 383 address/index probes (12.2%),
195 element loads, 70 value guards, 19 stores, 231 binding guards and 1,032
unattributed locations. The latter include ordinary instructions and allocator
moves outside named structural regions. These are CPU samples, not a count of
retired instructions. Raw code, maps, annotated assembly and the region tally
are under `profile-zlib-warm10/` and `zlib-warm10-regions.json`.

## Primary implementations

- [V8 typed element lowering](https://github.com/v8/v8/blob/main/src/compiler/js-native-context-specialization.cc),
  `BuildElementAccessForTypedArrayOrRabGsabTypedArray`: explicit bounds proof
  precedes typed load/store; element kind carries physical representation.
  An out-of-bounds path can join undefined, requiring a tagged result. Stores
  perform numeric conversion before the effect, with a separate clamping
  operation for Uint8Clamped. Otter must retain its canonical JS miss and
  resizable-buffer extent checks.
- [V8 load elimination](https://github.com/v8/v8/blob/main/src/compiler/load-elimination.cc),
  `AbstractElements::Lookup`: semantic receiver/index identity and compatible
  machine representation determine reuse. Aliasing writes invalidate facts.
  Source positions do not define memory-access equivalence.
- [JSC DFG effects](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/dfg/DFGClobberize.h),
  `GetIndexedPropertyStorage`: defines a location keyed by receiver and a
  memory region. Typed-array byte-offset reuse treats potentially resizable
  or growable shared views conservatively. Otter's own layout, reentry and
  moving-root rules remain the authoritative safety boundary.

## Replacement contract

1. The source HIR node owns a complete immutable element layout, copied with
   the node's identity when an inline body is mapped. Machine operations own
   that layout. Both emitters stop consulting the caller snapshot by byte PC.
2. GVN compares the complete program and SSA operands. View reuse stays in
   one block and one valid memory epoch. Raw base pointers are not loop
   invariants. Stores invalidate element values independently of metadata;
   reentrant and collecting operations invalidate the proof epoch.
3. Speculative loads produce Int32, Uint32 or Float64 directly. Signed narrow
   loads extend correctly; unsigned 32-bit values remain unsigned. Canonical
   fast/cold joins remain tagged because misses can produce any JS value.
4. Numeric indices stay scalar. Float64 indices require exact Uint32 round
   trip; negative signed indices fail before bounds. Fractions, NaN, infinity
   and overflow enter the original pre-effect exit or committed cold path.
5. Floating stores consume scalar doubles, narrowing Float32 at the memory effect.
   Integer stores keep low bits unboxed when the storage conversion is exact.
   Uint8Clamped retains its signedness-sensitive conversion. Canonical call
   arguments are boxed only inside the cold block and acquire ordinary roots.
6. The verifier checks storage/SSA representation compatibility. GC roots
   contain live tagged values; scalar deopt recipes retain their numeric
   representation. Successful stores cannot be replayed.

## Validation record

The first JIT run passed 282 tests and failed the new cross-target scalar
payload allocation test: x86 Element incorrectly used the complete C-call
clobber set. Its generated probe contract now names the actual GPR/FP scratch
set. The repeated JIT gate passed 283/283; x86-64 cross-check passed.

The new differential corpus covers signed/unsigned payloads, Float32/Float64
rounding, NaN and -0, numeric index misses, changed receiver kind, values live
across allocating calls, resizable-buffer shrink/regrow, and exactly-once
store coercion. Its standalone output matches Node. Captured IR proves that
putFloat/putDouble use a Float64 store operand. The event stream proves that
both typed element callees enter inlinePair and that its changed receiver
restores parent/callee frames (functions 518/517, resume PCs 13/2). The final
side-effect count is 2,002, not 2,003.

Preliminary adjacent zlib trials, before completing floating stores/inlining:
400,114,388,607 to 274,881,068,460 instructions (-31.30%); RSS 901,873,664 to
774,029,312 bytes (-14.18%). These are retained under element-reference and
element-first. Final all-workload measurements will supersede these trials.

The focused runtime suite initially stopped at artifact assertions in three
fixtures: warmup now inlined their element bodies, so no standalone callee
code object existed. These fixtures now own a one-iteration loop, preserving
their separate-entry/deopt purpose while the new corpus covers inlined exits.
The updated `jit_machine_elements` suite passed all seven tests.

## Final fixed-work measurement

All runs use `/usr/bin/time -l`, the stable toolchain and production tiering.
No build, artifact capture or sampling runs alongside these counters. Commands,
executable/script SHA-256 hashes and raw counters are in `element-final/`.
The before column is the immediately preceding free-list slice, not the original
pre-GC baseline. Node 24.16.0 and Bun 1.4.3-canary.1 references retain the same
input files and Bun Script wrapper as the previous report.

| Workload | Before instructions | After instructions | Change | Before / after RSS bytes | Otter/V8 ; Otter/JSC |
|---|---:|---:|---:|---:|---:|
| ts | 204,537,069,287 | 204,326,820,543 | -0.10% | 691978240 / 704036864 | 9.72× ; 9.86× |
| zlib | 399,851,223,485 | 274,896,946,207 | -31.25% | 902086656 / 788938752 | 11.88× ; 11.80× |
| crypto | 20,641,617,003 | 16,980,556,294 | -17.74% | 50626560 / 50413568 | 5.79× ; 7.63× |
| fib | 3,851,979,546 | 3,846,364,452 | -0.15% | 59588608 / 58966016 | 4.04× ; 7.13× |
| mega_method | 10,136,078,115 | 10,155,945,271 | +0.20% | 59883520 / 59473920 | 2.51× ; 6.21× |
| ast_ctor | 32,683,264,259 | 32,652,952,568 | -0.09% | 130285568 / 129843200 | 9.91× ; 15.85× |
| earley-boyer | 331,530,802,876 | 330,993,009,756 | -0.16% | 135217152 / 135331840 | 17.76× ; 15.09× |

The zlib instruction gain repeats the adjacent preliminary trials. Its final
RSS falls 12.54%. TS RSS increases 1.74%; no universal memory improvement is
claimed. The unseeded crypto row fixes iterations but not RNG operands; the
controlled variant below is required for attribution. Changes below 0.3% in the
other instruction rows are not treated as architectural wins.

Final validation:
- `cargo run --release -q -p otter-difftest`: 57 passed, 0 failed; production,
  Template, GC stress 1/4/16 match the interpreter.
- `cargo test --release -q -p otter-jit --lib`: 283 passed.
- `cargo test --release -q -p otter-runtime --test jit_machine_elements`: 7 passed.
- `cargo check --release -q -p otter-jit --tests --target x86_64-apple-darwin`:
  passed. x86 generated code was cross-compiled and allocator-tested, not executed
  on this ARM64 host. The existing unused detach-protector constant warning remains.

The remaining instruction ratios are 2.51–17.76× V8 and 6.21–15.85× JSC.
This is an indexed-access architecture improvement, not parity.

## Controlled repeats and compiler cost

The auxiliary crypto script prepends a fixed RNG before the original bytes:
32-bit LCG state `0x13579bdf`, multiplier 1664525, increment 1013904223,
unsigned wrap after every step, output divided by 4294967296. The original
agreed workload is unchanged. Variant SHA-256: `27620ff1994436c49d8d436b5e1bbd8bd11df0da6150501bda735af6c1b4823c`.

| Seeded crypto engine | Instructions | RSS bytes |
|---|---:|---:|
| Otter before | 20,536,492,593 | 50659328 |
| otter after | 17,027,358,413 | 50528256 |
| node after | 2,995,033,467 | 55803904 |
| bun after | 2,268,671,251 | 31031296 |

Controlled Otter instruction change: -17.09%.

The adjacent TS repeat is 203,899,461,651 → 203,827,242,433 instructions;
RSS 686,112,768 → 695,025,664 bytes (+1.30%). The memory increase survives
repetition and remains an explicit regression to investigate.

Separate event-only zlib runs (no artifact formatting, no sampling):

| Revision | Compile completions | Sum compile seconds | Generated code bytes |
|---|---:|---:|---:|
| before | 57 | 4.174609 | 16,268,012 |
| after | 57 | 3.872612 | 15,257,860 |

These durations include event capture and are wall-clock phase observations,
not the production retired-instruction measurement. The earlier artifact
baseline took about 17 seconds in compile callbacks because it also formatted
and captured code; that number must not be compared to event-only timings.

## Post-change attribution

The final artifact profile explicitly disables CLI timeout, retains stdout and
stderr, and checks Otter's successful exit. Sampling begins at 18 seconds.
`hotasm.py` now applies those checks to future runs. The earlier post-profile
at 8 seconds has only 66 mapped generated samples and is excluded.

The final capture maps 2,387 generated self samples: (unattributed) 852 (35.7%), machineElementView 578 (24.2%), machineElementAddress 390 (16.3%), machineBindingGuard 246 (10.3%), machineElementValueLoad 185 (7.8%), machineElementValueGuard 54 (2.3%).

Artifact capture reports truncation: 53 retained bundles, 2,136,787,329 retained
bytes and one dropped bundle (476,959,823 bytes). Consequently its 12,876,252
captured executable bytes are a partial sum. The complete 15,257,860-byte figure
above comes from the non-truncated event-only run, which has all 57 compile
completions. The profile only attributes mapped samples; these capture windows
are not production instruction counts or equal-work sample distributions.

## Next measured boundary

An uninstrumented successful Earley–Boyer run after this slice has 11,946
isolate self samples. OldSpace::alloc is 405 (3.39%), versus 2,141/12,404
(17.26%) before the free-list replacement. Generated code is 1,975 (16.53%).
Upvalue allocation, spine allocation and generated-upvalue initialization are
349 + 304 + 249 = 902 self samples (7.55%). Generated-upvalue initialization
has 894 inclusive samples, including 500 in cell allocation; closure allocation
has 731 inclusive, including 431 in spine allocation. Inclusive rows overlap
and must not be added. Collector costs remain substantial but cannot all be
attributed to captures from this profile alone.

The next architectural investigation is captured-binding storage: each own
binding currently allocates an old-space cell, followed by a separate closure
spine. Inspect grouped environment storage and trace ownership through frame,
closure, generated binding access and deopt before replacing this mechanism.
The measured TS RSS increase remains open alongside this next slice.
