# Old-space free-list replacement: measurements

Baseline: `1944cbee14c3ddff7c7bc815682b7a09d27db461`, Rust 1.97.1, Apple M1.
Raw counters, executable and script hashes: `benchmarks/results/parity-2026-09-27/`.
Each successful row is a single full fixed-work process, including startup and compilation.
Node 24.16.0; Bun 1.4.3-canary.1 (embedded JSC), with indirect eval for Script semantics.
The rejected direct Bun module runs are retained but excluded.

| Workload | Otter instructions | Node/V8 | Bun/JSC | RSS bytes Otter / Node / Bun |
|---|---:|---:|---:|---:|
| ts | 206,771,087,510 | 21,010,826,496 | 20,723,464,014 | 700547072 / 525910016 / 371326976 |
| zlib | 400,028,897,964 | 23,130,083,733 | 23,296,772,118 | 899448832 / 83312640 / 186138624 |
| crypto | 20,470,432,841 | 2,932,116,075 | 2,226,414,077 | 50757632 / 55918592 / 30916608 |
| fib | 3,866,033,397 | 952,937,757 | 539,223,922 | 59801600 / 50348032 / 19251200 |
| mega_method | 10,150,114,520 | 4,040,923,405 | 1,635,283,756 | 59670528 / 52297728 / 20381696 |
| ast_ctor | 33,205,462,954 | 3,295,764,054 | 2,060,623,753 | 130973696 / 53903360 / 30670848 |
| earley-boyer | 392,098,031,712 | 18,632,287,208 | 21,934,937,348 | 135561216 / 197738496 / 90275840 |

## Attribution before editing

Uninstrumented Earley–Boyer: 12,404 isolate self samples; OldSpace::alloc 2,141
(17.26%), BinaryHeap::pop for FreeEntry 409 (3.30%), generated code 1,785 (14.39%).
This is the largest individually identified avoidable allocator family in the
highest-ratio workload. TS intrinsic forwarding is 1,557 inclusive samples out
of 12,215; that includes the target work and cannot all be attributed to forwarding.
Zlib is mostly generated code (8,889/12,538), with optimizing compilation inclusive
2,549/12,538. These are sampling shares, not retired instruction counts.

Artifact captures for all three workloads and hotasm annotations are retained.
Artifact-enabled TS/zlib profiles spend substantial time formatting artifacts;
only the separate `plain-*` runs establish production CPU attribution.

## Mechanism

Replace native Vec/BinaryHeap hole storage with intrusive links in dead filler
payloads. Keep exact size classes below 256 bytes and doubling classes above.
Use a bitmap to skip empty classes. Probe one head in the request class, then
the smallest larger nonempty class, as V8 FreeListManyCached does. Keep the
existing linear allocation area to amortize list refills. No hole sorting,
native allocation per hole, or walking empty classes survives.

The collector clears lists before sweep and only lists ranges in surviving pages.
Filler headers remain walkable. Links are cage offsets into nonmoving dead old
ranges; no live or young payload contains allocator metadata. Promotion preflight
and all moving-root and write-barrier rules remain authoritative.

Primary sources: [V8 free-list implementation](https://github.com/v8/v8/blob/main/src/heap/free-list.cc),
[V8 allocator contract](https://github.com/v8/v8/blob/main/src/heap/free-list.h),
[JSC free intervals](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/heap/FreeList.h).
Unlike a max-heap, a class head can hide another fitting range in the same class.
Measure RSS and page churn to evaluate this fragmentation tradeoff.

## Validation and after measurements

| Workload | Before instructions | After instructions | Change | Before / after RSS bytes | Otter/V8 ; Otter/JSC |
|---|---:|---:|---:|---:|---:|
| ts | 206,771,087,510 | 204,537,069,287 | -1.08% | 700547072 / 691978240 | 9.73× ; 9.87× |
| zlib | 400,028,897,964 | 399,851,223,485 | -0.04% | 899448832 / 902086656 | 17.29× ; 17.16× |
| crypto | 20,470,432,841 | 20,641,617,003 | +0.84% | 50757632 / 50626560 | 7.04× ; 9.27× |
| fib | 3,866,033,397 | 3,851,979,546 | -0.36% | 59801600 / 59588608 | 4.04× ; 7.14× |
| mega_method | 10,150,114,520 | 10,136,078,115 | -0.14% | 59670528 / 59883520 | 2.51× ; 6.20× |
| ast_ctor | 33,205,462,954 | 32,683,264,259 | -1.57% | 130973696 / 130285568 | 9.92× ; 15.86× |
| earley-boyer | 392,098,031,712 | 331,530,802,876 | -15.45% | 135561216 / 135217152 | 17.79× ; 15.11× |

The ratio column lists Otter/Node and Otter/Bun; both must approach 1 for parity.
These remain single-run process counters, not confidence intervals. The
crypto repeats were 20,716,576,950 / 20,660,521,749 / 20,646,614,692
instructions. Its harness fixes iteration count but initializes the RSA RNG
from `Math.random()` before entering the fixed loop, so actual operand work
varies. The observed increase of about 1% is unresolved; it is not evidence
of an allocator improvement or a controlled regression measurement. A seeded
input run is required for tighter attribution in the next slice.
Earley–Boyer's independent first trial was 331,569,261,934 instructions,
135,413,760 RSS bytes; the final trial reproduces the 15.45% instruction gain.
The RSS change in that workload is small, so this is not a memory-parity claim.
No JIT emission or tier policy changed in this slice.

Validation on the final source:
- `cargo test --release -q -p otter-gc --lib space::`: 12 passed.
- `cargo run --release -q -p otter-difftest`: 56 passed, 0 failed;
  normal tiering, Template and GC stress strides 1, 4, 16 match interpreter.
- `cargo test --release -q -p otter-jit --lib`: 281 passed.
- All seven fixed-work programs completed successfully.

The intrusive list is also cleared inside `reap_dead_pages` before releasing
any page. Direct users of that operation cannot leave links into released cage
pages; the regression test allocates again after reaping two linked pages.

Next measured target: zlib indexed-element view proofs, which retain byte-PC
identity instead of complete semantic layout identity in Machine IR. Existing
artifact samples attribute 97 of 286 generated-code self samples to
`machineElementView`; the attempted 30-second-delayed sample missed the isolate because execution
had already completed. It is excluded; the next capture will start earlier. Compiler temporary arenas remain a separate hypothesis.
